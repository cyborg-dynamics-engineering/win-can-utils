# Windows CAN Utils

This project aims to provide a Windows equivalent of [can-utils](https://github.com/linux-can/can-utils), offering CLI tools for interacting with CAN devices via USB-to-CAN transceivers. It currently supports slcan adapters, including the DSD Tech [SH-C30A](https://www.deshide.com/product-details_SH-C30A.html) and [SH-C31A](https://www.deshide.com/product-details_SH-C31A.html), both of which are widely available on Amazon for under $20 USD. More devices may be added in the future.

## Table of Contents
- [Windows CAN Utils](#windows-can-utils)
  - [Table of Contents](#table-of-contents)
  - [Installation](#installation)
  - [Usage](#usage)
    - [CAN Server](#can-server)
    - [CAN Dump](#can-dump)
    - [CAN Send](#can-send)
  - [slcan firmware installation](#slcan-firmware-installation)
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

## slcan firmware installation
SH-C30A and SH-C31A may not ship with slcan firmware. The stock CANable slcan firmware can be installed using the online canable updater tool: https://canable.io/updater/.
* SH-C30A requires CANable v1.0 'slcan' firmware: https://canable.io/updater/canable1.html.
* SH-C31A requires CANable v2.0 'slcan with FD support' firmware: https://canable.io/updater/canable2.html

To flash the device, perform the following steps:
1. Unplug the adapter from your computer.
2. Switch the boot toggle into the 'down' position as pictured.
3. Plug the adapter into your computer.
4. Open the canable updater page and select 'Connect and Update'.
5. Choose the CAN adapter from the list. It should appear as 'DFU in FS Mode - Paired'.
6. Wait for the flash to complete. An error message may appear, but can be ignored.

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
