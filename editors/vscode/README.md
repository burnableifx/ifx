# IFX development adapter

Build the Rust server from the repository root (`cargo build -p ifx-lang`). Set
`ifx.serverPath` in VS Code's user settings to the absolute path of the built
`ifx-lang` binary, or put it on PATH. The setting is machine-scoped.

Install the adapter's pinned dependency locally, then launch a development host:

```sh
cd editors/vscode
npm ci
code --extensionDevelopmentPath="$PWD" ../../examples/language
```

Open `.ifx` files under the project workspace. The server reads modules declared
in `Ifx.toml` and already-fetched Git dependencies; those files need not be open.
Try `use shared::` completion, a trailing `.`, an invalid
`.region(42)`, hover, go to definition on a local variable, and formatting.

This adapter contains no language semantics. It starts `ifx-lang lsp` through the
standard VS Code language client. It does not compile Rust, invoke `burn`, or run
configuration or fetch dependencies. Save manifest changes to update module scope.
Workspace trust is required to start the configured executable.

The adapter's syntax/package have been checked. The server has passed a real
Neovim session and a stdio process test; a VS Code UI session remains unverified
because VS Code was not installed on the implementation host.
