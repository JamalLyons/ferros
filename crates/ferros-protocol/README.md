# ferros-protocol

Communication layer between Ferros debugger and frontend.

## Overview

`ferros-protocol` defines the structured communication protocol used between the Ferros debugger backend and frontend clients, enabling:

- Remote debugging capabilities
- Structured message passing
- Protocol versioning for backward compatibility
- Efficient serialization (JSON or binary formats)

## Usage

Add `ferros-protocol` to your `Cargo.toml`:

```toml
[dependencies]
ferros-protocol = "0.0.0"
```

## Features

- Structured message definitions
- Serialization support (JSON, MessagePack)
- Protocol versioning
- Remote debugging protocol

## License

Licensed under the Apache License, Version 2.0. See the [repository](https://github.com/jamallyons/ferros) for details.

