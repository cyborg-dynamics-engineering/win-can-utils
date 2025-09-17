# Windows CAN Utils

Provides CLI tools for interacting with Cyder CAN modules over serial. We currently target the [DSD TECH SH-C30A](https://www.deshide.com/product-details_SH-C30A.html) USB to CAN adapter on Windows devices.


## Install
```
cd win_can_utils
cargo install --path .
```

## Generating MSI Installer
To install cargo wix:
```
cargo install cargo-wix
```

Download the WiX3 binaries zip - [WiX3 toolset](https://github.com/wixtoolset/wix3/releases)<br>
Extract it somewhere on your PC. ie: C:\Program Files\WiX Toolset v4.0\bin\
Add the directory containing the binaries to PATH in Environment Variables.

Generate MSI file:
```
cargo wix
```

## CAN Server
Opens a serial line CAN connection to a given COM port and exposes it to a Windows [pipe](https://learn.microsoft.com/en-us/windows/win32/ipc/pipes).
```
can_server COM5
```

A bitrate can be optionally set. If not provided, the server will attempt to measure the existing bitrate on the bus.
```
can_server COM3 1000000
```


## CAN Dump
Prints realtime CAN data from an open CAN pipe to standard output.
```
can_dump COM5
```


## CAN Send
Sends the given CAN frame to an open CAN pipe.

## Usage
The COM port must be specified to the executable, followed by the CAN frame in the format: ID#DATA
```
can_send COM5 055#00
```

## License
Windows CAN Utils is licensed under either of

    Apache License, Version 2.0 (LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0)
    MIT license (LICENSE-MIT or http://opensource.org/licenses/MIT)

at your option.


## Contribution
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
