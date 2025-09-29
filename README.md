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
  - [License](#license)
  - [Developers](#developers)
    - [Contribution](#contribution)
    - [Install from source files (Using Cargo)](#install-from-source-files-using-cargo)
    - [Generating MSI Installer](#generating-msi-installer)


## Installation

1. Download the latest `.msi` installer from the
   [Releases](https://github.com/Cyborg-Dynamics-Engineering/win-can-utils/releases) page.
2. Run the installer and follow the on-screen prompts to complete setup.
3. Install the `PCANBasic.dll` runtime dependency (see
   [PCAN-Basic dependency](#pcan-basic-dependency)).

> **Tip:** Administrative privileges are recommended for installing both the
> application and the supporting drivers.

## PCAN-Basic dependency

Some CAN adapters require the
[PCAN-Basic API](https://www.peak-system.com/PCAN-Basic.239.0.html?&L=1)
runtime library to be present on the system. To install the dependency:

1. Download the latest PCAN-Basic package from PEAK-System.
2. Review and accept the PEAK-System PCAN-Basic End User License Agreement
   (EULA) during installation. Installation and use of `PCANBasic.dll`
   indicates that you agree to the terms of the EULA; ensure that your use
   complies with all PEAK-System licensing requirements.
3. Extract `PCANBasic.dll` from the package and copy it into the same
   directory as the Windows CAN Utils executables (or another directory on the
   system `PATH`).
4. Restart any running Windows CAN Utils processes to ensure the new DLL is
   loaded.

You only need to perform these steps once per machine, unless you remove or
update the DLL in the future.

## Usage
### CAN Server
Opens a serial line CAN connection to an slcan adapter and exposes it via Windows [pipe](https://learn.microsoft.com/en-us/windows/win32/ipc/pipes).<br>
To find the adapter, open 'Device Manager' and look in the 'Ports (COM & LPT)' dropdown.
```
Usage: canserver <driver> <port> [--bitrate <bitrate>]
Example: canserver slcan COM5 --bitrate 1000000
```

### CAN Dump
Prints realtime CAN data from an open CAN pipe to standard output.
```
Usage: candump <port>
Example: candump COM5
```
NOTE: A CAN server instance **must** be open for the target port.

### CAN Send
Sends the given CAN frame to an open CAN pipe.
```
Usage: cansend <port> <ID#DATA>
Example: cansend COM5 055#00
```
NOTE: A CAN server instance **must** be open for the target port.

## slcan firmware installation
SH-C30A and SH-C31A may not ship with slcan firmware. The stock CANable slcan firmware can be installed using the online canable updater tool: https://canable.io/updater/.
* SH-C30A requires CANable v1.0 'slcan' firmware: https://canable.io/updater/canable1.html.
* SH-C31A requires CANable v2.0 'slcan with FD support' firmware: https://canable.io/updater/canable2.html

To flash the device, perform the following steps:
1. Unplug the adapter from your computer.
2. Put the boot switch into the 'On' position as pictured.
   
&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<img src="https://github.com/user-attachments/assets/154c4837-61d0-402f-9a38-76f50d5a5f81" width="200">

4. Plug the adapter into your computer.
5. Open the canable updater page and select 'Connect and Update'.
6. Choose the CAN adapter from the list. It should appear as 'DFU in FS Mode - Paired'.
7. Wait for the flash to complete. An error message may appear, but can be ignored.
8. Put the boot switch into the 'Off' position.
9. Unplug and re-insert the adapter into the PC.
10. Attempt to open a can_server using the device to determine whether flashing was successful.

## License
Windows CAN Utils is licensed under either of Apache License, Version 2.0 (LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0) or MIT license (LICENSE-MIT or http://opensource.org/licenses/MIT) at your option.

## Developers

### Contribution
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

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
