# IFX language: testable MVP

`ifx-lang` implements the experimental `ifx/0.1-experimental` grammar in Rust. The
same parser and evaluator power CLI checks, compilation and the stdio language
server. Configuration runs against an in-memory host only. Existing Rust stacks
and IFXD execution are unchanged; this is not yet wired into `burn` or real host
execution.

The [function/struct language proposal](language-next.md) refines this surface with
ordinary `let` bindings, typed parameters, `impl`/`new`, explicit returns and
`.key(...)` identity. Its [examples](../examples/language/proposal/README.md) are
design material and do not run on this implementation.

## Try it

The [numbered infrastructure examples](../examples/language/README.md) show single
servers, reusable environments, a Linode application stack, AWS networking and
application lifecycle configuration. Each distinguishes implemented behavior from
proposed resource or host APIs.

From the repository root:

```sh
cargo build -p ifx-lang
cargo run -p ifx-lang -- check examples/language/linode.ifx
cargo run -p ifx-lang -- check examples/language/modules.ifx
cargo run -p ifx-lang -- check examples/language/Ifx.toml
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
statement  = binding | use | import | for | if | expression, ";" ;
binding    = ("let" | "resource" | "module" | "input" | "output"), identifier,
             [":", type], "=", expression, ";" ;
use        = "use", identifier, "::", identifier, {"::", identifier},
             ["as", identifier], ";" ;
import     = "import", identifier, "from", string, ";" ; (* legacy *)
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
mutation, `while`, imports containing URLs or implicit package downloads. Lambdas are accepted
by `.configure` and the three action-policy methods only.

## Resources and modules

```text
use crate::machine;
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

`use` declarations belong at module scope. The last path segment becomes the local
binding; `as` gives it a different name. `use` imports a module definition, and
`module instance = definition("stable-key")...` instantiates it. There are no globs,
grouped imports, `pub use`, or implicit source-directory scanning in this increment.

### Projects and shareable packages

[The working manifest](../examples/language/Ifx.toml) and
[shared-package example](../examples/language/06-shared-modules.ifx) demonstrate:

```rust
use crate::machine;
use shared::web as web_server;

module app = web_server("application")
    .name("production-web")
    .region("us-east");
output website: String = app.url;
```

```toml
# Ifx.toml in the consuming project
[package]
name = "production"
version = "0.1.0"
entry = "main.ifx"

[modules]
machine = "modules/machine.ifx"

[dependencies]
shared = { path = "shared" }
```

`crate::` refers to the current package's `[modules]`. Other first segments are
aliases explicitly listed in `[dependencies]`; a dependency cannot see consumer
modules or other dependencies. The source file itself is a parameterized module:
its existing `input` and `output` declarations form the interface. An entry file
is optional for a library package. Select an exported `.ifx` file to check a library;
checking/compiling `Ifx.toml` requires `package.entry`. Version is informational in
this increment; there is no version solver.

The shared directory can become its own Git repository. Its root manifest lists
its available exports:

```toml
[package]
name = "shared_infrastructure"
version = "0.1.0"

[modules]
"compute::server" = "src/server.ifx"
web = "src/web.ifx"
```

Within `src/web.ifx`, `use crate::compute::server;` imports the other exported file.
Consumers can use `shared::compute::server`. Package identity and consumer alias
are separate; deployed identity still comes from stable module/resource keys.
All exports are public to consumers; private modules/re-exports are not implemented.

To use Git, replace the local dependency with an HTTPS URL and an actual complete
40-character lowercase commit SHA. This is a template, not a fetchable example:

```toml
[dependencies]
shared = { git = "https://github.com/YOUR_ORG/ifx-modules.git", rev = "FULL_40_CHARACTER_COMMIT_SHA" }
```

Then run explicitly:

```sh
ifx-lang fetch --manifest-path Ifx.toml
ifx-lang check Ifx.toml
ifx-lang compile main.ifx
```

`fetch` writes `Ifx.lock` and extracts the manifest and declared `.ifx` exports into
`.ifx/deps/<sha256-of-url-and-revision>/`. Commit `Ifx.toml` and `Ifx.lock`; ignore
`.ifx/`. The lock records the URL, exact commit and SHA-256 of every extracted file.
Changing the URL/revision requires fetching again. Checks reject stale locks,
missing cache files and modified cached contents; analysis uses the same bytes it
verified. Refetching restores the pinned files. A failed fetch preserves the old
lock; previously completed cache writes may remain. Old cache entries are not
pruned automatically. Hashes detect changes relative to the lock; they do not
establish publisher authenticity if the lock itself is changed.

Fetching uses Git objects directly, with no working-tree checkout, hooks, filters,
build scripts, submodules or configuration execution. Ambient Git configuration,
credential helpers, redirects and non-HTTPS transports are disabled. The current
fetcher therefore supports public HTTPS repositories; private credentials, SSH,
floating branches/tags, registry lookup and nested package dependencies remain
future work. Local dependency paths must stay beneath the project directory:
parent/absolute/hidden paths and symlinks are rejected. Shared dependency packages
must be self-contained but can compose their own exports.

The fetch command currently targets Linux with `/usr/bin/git` and `/usr/bin/prlimit`.
Each command has a 60-second deadline, including output-pipe completion; cleanup
kills its process group. Inherited limits are 64 MiB per file, 512 MiB address space,
30 CPU seconds and 64 file descriptors. A sampled 64 MiB/1,024-entry temporary
repository budget also stops oversized transfers; this is not a hard aggregate
disk quota. Git tree/blob output is capped at 256 KiB; an extracted package at 1 MiB.
These limits intentionally favor small module repositories.

CLI checks find the nearest `Ifx.toml` above a selected source; passing the manifest
selects its entry. Legacy `import name from "./file.ifx";` remains accepted for the
original standalone fixtures. Without a manifest, CLI legacy imports are confined
to the entry directory, and the LSP uses explicitly open buffers only. With a
manifest, even legacy imports can access only files already in the declared scope.
Files are opened through directory descriptors with no-follow/nonblocking flags
and must be regular files. Module errors identify the call site and child path/byte
offset; cross-file go-to-definition remains a later increment.

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
sync, UTF-16 positions, diagnostics, use-path/member/value/name completion, schema hover,
local go-to-definition, document symbols and conservative whitespace formatting.
Formatting preserves existing vertical chains and comments; it is not yet a full
opinionated reformatter. Requests are synchronous and bounded, without background
jobs; cancellation notifications do not interrupt an active computation.

Neovim 0.11+ configuration is in `editors/ifx.lua`. Put the built binary on PATH
or change `cmd` to its absolute path. Initialize the client with the project workspace
root. The server discovers the nearest manifest within that boundary and reads
only its declared sources and verified cached dependencies. Directory symlinks are
rejected during discovery. Open local buffers override saved source; closing them
restores saved content. Saving or watched-file notifications recheck consumers
(the VS Code adapter watches manifests, locks and `.ifx` files). Manifest edits must
be saved before they change scope; unsaved TOML analysis is not implemented.
Typing performs no commands, provider calls, secret resolution or Git fetching.
The compiler, editor and simulator do not start a local IFXD.

A thin [VS Code adapter](../editors/vscode/README.md) starts the same Rust server
using the standard language client. Its package and JavaScript syntax are checked;
an end-to-end VS Code UI session remains unverified on this host. The CLI's bounded
file loader targets Unix; Git fetching currently requires Linux. Windows packaging is not included.

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
Project component tests cover offline package scope, real local Git object extraction,
lock/cache round trips, tamper detection, symlink boundaries and helper termination.
They do not contact an HTTPS server; live network transport acceptance remains unverified.

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
