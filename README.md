# Windows CAN Utils

Windows CAN Utils provides a Windows-compatible equivalent of [can-utils](https://github.com/linux-can/can-utils), offering command-line tools to interact with CAN devices via USB-to-CAN adapters.  

Currently supported adapters include:
- **CANable adapters** (best tested with the [DSD Tech SH-C30A](https://www.deshide.com/product-details_SH-C30A.html))  
- **PCAN adapters**

> **⚠️ Note:** If this project interests you, check out our related project [CyderVis](https://github.com/cyborg-dynamics-engineering/cyder-vis) for CAN data visualization.

## Table of Contents
- [Windows CAN Utils](#windows-can-utils)
  - [Installation](#installation)
  - [PCAN-Basic Dependency](#pcan-basic-dependency)
  - [Usage](#usage)
    - [CAN Server](#can-server)
    - [CAN Dump](#can-dump)
    - [CAN Send](#can-send)
  - [Canable Firmware Installation](#canable-firmware-installation)
  - [Installing WinUSB Driver for Canable Devices](#installing-winusb-driver-for-canable-devices)
  - [License](#license)
  - [Developers](#developers)
    - [Contribution](#contribution)
    - [Install from Source (Cargo)](#install-from-source-cargo)
    - [Generating an MSI Installer](#generating-an-msi-installer)

## Installation

1. Download the latest `.msi` installer from the [Releases](https://github.com/Cyborg-Dynamics-Engineering/win-can-utils/releases) page.  
2. Run the installer and follow the setup prompts.  
3. If using a PEAK-System adapter, install the optional `PCANBasic.dll` runtime dependency (see [PCAN-Basic Dependency](#pcan-basic-dependency)).  

> **Tip:** Run the installer with **administrator privileges** to avoid driver or permission issues.

## PCAN-Basic Dependency

PEAK-System CAN adapters require the [PCAN-Basic API](https://www.peak-system.com/PCAN-Basic.239.0.html?&L=1) runtime library.  
Other adapters do not require this dependency.  

**To install:**

1. Download the latest PCAN-Basic package from PEAK-System.  
2. Accept the End User License Agreement (EULA).  
   - Installing or using `PCANBasic.dll` implies acceptance of the terms.  
3. Extract `PCANBasic.dll` and place it in:  
   - The same directory as the Windows CAN Utils executables, **or**  
   - Any directory included in your system `PATH`.  
4. Restart Windows CAN Utils processes to ensure the DLL is loaded.  

> **Disclaimer:** Windows CAN Utils is not affiliated with or endorsed by PEAK-System Technik GmbH. Ensure compliance with their license terms.

## Usage

### CAN Server
Opens a CAN connection to a USB-to-CAN adapter and exposes it via a Windows [pipe](https://learn.microsoft.com/en-us/windows/win32/ipc/pipes).  
The interface can be auto-detected or specified manually. **Bitrate usually must be specified** unless supported auto-detect exists.

```
Usage: canserver <driver> [--channel <channel> --bitrate <bitrate>]
Example: canserver gsusb --bitrate 1000000
```

Supported drivers:
- `gsusb` → CANable / candleLight adapters (gs_usb protocol)  
- `slcan` → Serial-line CAN adapters  
- `pcan` → PEAK PCAN-USB/PCI/LAN adapters (requires [PCAN-Basic Dependency](#pcan-basic-dependency))  

### CAN Dump
Displays real-time CAN traffic from an open CAN pipe. 
```
Usage: candump <port>
Example: candump can0
```
⚠️ Requires an active CAN server instance for the target port.

### CAN Send
Sends the given CAN frame to an open CAN pipe.
```
Usage: cansend <port> <ID#DATA>
Example: cansend COM5 055#00
```
⚠️ Requires an active CAN server instance for the target port.

## Canable Firmware Installation

Some Canable devices may not ship with the correct firmware.  
We provide a flashing tool for devices using the **STM32F072 microcontroller** (e.g., the [DSD Tech SH-C30A](https://www.deshide.com/product-details_SH-C30A.html)).  

- **Our Tool:** https://cyborg-dynamics-engineering.github.io/canable-flasher/
  - Installs **canable-fw v2.0**, the latest candleLight firmware  
  - Works only with **STM32F072-based devices**  

- **Alternate Tool:** [Canable Updater](https://canable.io/updater/)  
  - Wider device compatibility  
  - Exact firmware build is **uncertain**  

⚠️ Both tools rely on **WebDFU**, which is supported only in **Google Chrome**.

### Flashing Steps
1. Unplug the adapter from your PC.  
2. Toggle the **boot switch to `On`** (see picture).  

   <img src="https://github.com/user-attachments/assets/154c4837-61d0-402f-9a38-76f50d5a5f81" width="200">

3. Plug the adapter back in.  
4. Open the [Canable Flasher](https://cyborg-dynamics-engineering.github.io/canable-flasher/) and flash the firmware.  
   - If the device does not appear, see [Installing WinUSB Driver](#installing-winusb-driver-for-canable-devices).  
5. Wait for the flash to complete. (Ignore non-critical error messages.)  
6. Set the boot switch back to **`Off`**.  
7. Unplug and reconnect the adapter.  
8. Test by starting a `can_server` session to verify success.  

## Installing WinUSB Driver for Canable Devices

Canable devices on Windows use the **WinUSB driver**.  
This may need to be installed manually, especially when in bootloader mode.

### Steps

1. Open **Device Manager**  
   - Press **Win+R**, type `devmgmt.msc`, and press **Enter**.  

2. Locate the device  
   - If missing a driver, it will appear under **Other devices**.  

   ![Device Manager - Other Devices](https://github.com/user-attachments/assets/a7f27467-ffa5-4740-ac4f-305da9a87bde)

3. Install WinUSB  
   - Right-click device → **Update Driver**  
   - Choose:  
     `Browse my computer → Let me pick → Universal Serial Bus devices → WinUsb Device → Next`  

   ![Update Driver - WinUSB Selection](https://github.com/user-attachments/assets/74af6acf-31b7-42fa-9852-2f720b5f36ce)

4. Verify installation  
   - The device should now appear under **Universal Serial Bus devices**.

## License

Windows CAN Utils is dual-licensed under:  
- [Apache License 2.0](http://www.apache.org/licenses/LICENSE-2.0)  
- [MIT License](http://opensource.org/licenses/MIT)  

You may choose either license.

## Developers

### Contribution
By submitting a contribution, you agree it will be dual-licensed under the same terms (Apache 2.0 and MIT), without additional restrictions.

### Install from Source (Cargo)
```
cd win_can_utils
cargo install --path .
```

### Generating an MSI Installer

1. Install cargo-wix:  
```
cargo install cargo-wix
```
2. Download WiX3 binaries from: https://github.com/wixtoolset/wix3/releases  
- Extract (e.g., `C:\Program Files\WiX Toolset v4.0\bin\`)  
- Add the bin directory to your **PATH** environment variable  

3. Generate the MSI file:  
```
cargo wix
```
