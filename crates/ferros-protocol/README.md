# ferros-protocol

Communication layer between the Ferros debugger core and frontend clients.

## Status

`ferros-protocol` is currently an empty workspace member reserved for future protocol and remote debugging support. No code has been implemented in this crate yet.

## Planned Scope

When implemented, `ferros-protocol` will define structured communication schemas between the `ferros-core` debugging engine and external clients (CLI, TUI, IDEs, remote daemons):

- Structured event and command serialization (JSON and binary formats)
- Debug Adapter Protocol (DAP) compatibility for VS Code and other editors
- Remote debugging over TCP or Unix domain sockets
- Multi-client and daemon session management

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.
