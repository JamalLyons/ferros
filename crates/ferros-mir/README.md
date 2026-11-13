# ferros-mir

MIR integration, introspection, and analysis for Ferros.

## Status

`ferros-mir` is currently an empty workspace member reserved for future compiler IR integration. No code has been implemented in this crate yet.

## Planned Scope

When implemented, `ferros-mir` will provide Rust-specific debugging capabilities by integrating with the Rust compiler's Mid-level Intermediate Representation (MIR):

- MIR block traversal and control-flow analysis
- Correlating machine code instructions with MIR basic blocks
- Type and lifetime inspection based on compiler metadata
- Integration with `rustc_driver` or `rustc_private`
- Symbolic execution and MIR interpretation (drawing inspiration from Miri)

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.
