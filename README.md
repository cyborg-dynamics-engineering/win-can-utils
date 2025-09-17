# Windows CAN Utils

Provides CLI tools for interacting with Cyder CAN modules over serial. We currently target the [DSD TECH SH-C30A](https://www.deshide.com/product-details_SH-C30A.html) USB to CAN adapter on Windows devices.

- [Windows CAN Utils](#windows-can-utils)
  - [Installation](#installation)
  - [Usage](#usage)
    - [CAN Server](#can-server)
    - [CAN Dump](#can-dump)
    - [CAN Send](#can-send)
  - [Developers](#developers)
    - [Install from source files (Using Cargo)](#install-from-source-files-using-cargo)
    - [Generating MSI Installer](#generating-msi-installer)
  - [License](#license)
  - [Contribution](#contribution)


## Installation
Download & run the latest .msi installer from [Releases](https://github.com/Cyborg-Dynamics-Engineering/win-can-utils/releases)

## Usage
### CAN Server
Opens a serial line CAN connection to an slcan adapter and exposes it via Windows [pipe](https://learn.microsoft.com/en-us/windows/win32/ipc/pipes).<br>
To find the adapter, open 'Device Manager' and look in the 'Ports (COM & LPT)' dropdown.
```
Usage: can_send <port> [--bitrate <ID#DATA>]
Example: can_server COM5 --bitrate 1000000
```

### CAN Dump
Prints realtime CAN data from an open CAN pipe to standard output.
```
Usage: can_dump <port>
Example: can_dump COM5
```
NOTE: A CAN server instance **must** be open for the target port.

### CAN Send
Sends the given CAN frame to an open CAN pipe.
```
Usage: can_send <port> <ID#DATA>
Example: can_send COM5 055#00
```
NOTE: A CAN server instance **must** be open for the target port.

## Developers

### Install from source files (Using Cargo)
```
cd win_can_utils
cargo install --path .
```

### Generating MSI Installer
To install cargo wix:
```
cargo install cargo-wix
```

Download the WiX3 binaries .zip - https://github.com/wixtoolset/wix3/releases<br>
Extract it somewhere on your PC. ie: C:\Program Files\WiX Toolset v4.0\bin\
Add the directory containing the binaries to PATH in Environment Variables.

Generate MSI file:
```
cargo wix
```

## License
Windows CAN Utils is licensed under either of

    Apache License, Version 2.0 (LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0)
    MIT license (LICENSE-MIT or http://opensource.org/licenses/MIT)

at your option.


## Contribution
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
