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
    - [Windows Network Adapter (Experimental)](#windows-network-adapter-experimental)
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
Runs a named, multi-client CAN interface broker over local Windows pipes. Each
subscriber has independent filters, a bounded receive queue, and drop counters.
Settings and desired up/down state are saved between runs. The broker remains
available for configuration and status when hardware is down or disconnected.

```powershell
canctl can0 create --driver gsusb --device SERIAL/0 --bitrate 500000
canserver --name can0
# From another terminal:
canctl can0 up
candump -d can0
cansend can0 123#010203
canctl can0 show
canctl can0 clients
canctl can0 down
canctl can0 set --bitrate 250000
canctl can0 up
```

Configurations default to `%LOCALAPPDATA%\win-can-utils\interfaces\NAME.json`.
Use `--config PATH` for another location. `canserver virtual --name vcan0`
provides a hardware-free interface. Keep `canserver` running while clients use
it; this does not install a Windows service or create an NDIS network adapter.

The versioned duplex client is exposed through
`crosscan::service::ServiceClient`; legacy `WindowsCan` clients remain supported.
See the [v1 protocol and lifecycle guide](docs/client-protocol-v1.md) for wire
format, client APIs, timestamps, capabilities, and echo semantics, and the
[Python client example](examples/service_client.py) for another language.

The original creation syntax is also supported and saves the configuration.
The interface can be auto-detected or specified manually. **Bitrate usually must be specified** unless supported auto-detect exists.

```
Usage: canserver <driver> [--channel <channel> --bitrate <bitrate>]
Example: canserver gsusb --bitrate 1000000
```

Supported drivers:
- `gsusb` → CANable / candleLight adapters (gs_usb protocol)  
- `slcan` → Serial-line CAN adapters  
- `pcan` → PEAK PCAN-USB/PCI/LAN adapters (requires [PCAN-Basic Dependency](#pcan-basic-dependency))  

For gs_usb, `canserver gsusb --list-adapters` lists connected adapters, including
ones that are busy, inaccessible, or have an unsupported USB driver. Starting
with `canserver gsusb --bitrate 1000000` opens the sole available adapter when
only one is connected. When several are connected, it displays the list and
prompts for an available adapter. Scripts must select explicitly with `--device`.

Run a separate server for each adapter, with independent IPC names:

```powershell
canserver gsusb --list-adapters
canserver gsusb --device 003:014/0 --name can0 --bitrate 1000000
# In another terminal:
canserver gsusb --device 003:015/0 --name can1 --bitrate 1000000
```

Use the IDs from your own list; USB addresses can change after reconnection.
`--device` also accepts a serial number or product name (optionally `/interface`).
When `--name` is omitted, servers reserve the next free `canN` atomically.
Another server cannot claim the same adapter interface or IPC name. Each selected
USB interface must expose gs_usb bulk endpoints; a DFU interface is not a CAN
interface. Legacy `--channel` selectors remain supported.

The gs_usb receive path keeps 64 asynchronous USB reads queued. USB completion
wakes the receiver immediately; an idle `canserver` waits for a frame notification
outside the driver lock, leaving transmit and control commands available. It does
not add a sleep between CAN frames. Completed reads are parsed in submission order
to preserve frame fragments and hardware timestamps.

Each subscriber's bounded IPC queue drops new frames for that subscriber when
full, without blocking hardware reception or other clients. Inspect counters
with `canctl NAME clients` or `candump -d NAME`. Hardware and USB buffers remain
finite; server queue drops and client-side drops are reported separately.

Transmits queue up to ten asynchronous USB writes in submission order. A credit
returns only after both USB completion and its firmware echo. `send_frame`
acknowledges queue admission; later USB failures stop the worker and surface
through receive or subsequent send operations. CAN mode changes drain earlier
USB writes, and dropping a driver cancels and retires its pending transfers.
FD-capable adapters use compact receive records for classic CAN. Classic transmit
records use 20 bytes, with a 76-byte compatibility quirk for the tested factory
SH-C31x firmware. The Windows USB backend is patched locally to claim only the selected
interface; see [the vendor notes](vendor/libusb1-sys/LOCAL_CHANGES.md).
Hardware measurements and a reproducible load test are documented in
[gs_usb validation](docs/gs-usb-validation.md).

### CAN Dump
Displays real-time traffic using a dedicated subscription. Multiple `candump`
instances can observe the same interface, with separate filters and queues.
```
Usage: candump <port>
Example: candump can0
```

```powershell
candump -d --queue-capacity 4096 "can0,123:800007FF"
```
⚠️ Requires an active CAN server instance for the target port.

### CAN Send
Sends the given CAN frame to an open CAN pipe.
```
Usage: cansend <port> <ID#DATA>
Example: cansend COM5 055#00
```

Successful sends are echoed to other matching subscribers at backend admission.
`cansend can0 123#01 --receive-own-messages` also receives and prints the sender's
own echo. Echo is not proof of a CAN acknowledgement. Standard, extended and RTR
classic frames are supported; end-to-end CAN FD remains unavailable even when
the adapter advertises FD hardware capability.
⚠️ Requires an active CAN server instance for the target port.

### Windows Network Adapter (Experimental)

`canadapter` bridges a dedicated Wintun NDIS network adapter to a CAN service
interface. Applications use normal UDP sockets and the v1 JSON protocol through
that adapter. Read-only diagnostics work without elevation:

```powershell
canadapter probe --dll "C:\path\to\wintun.dll"
```

Adapter creation and IP setup require an elevated run. The prototype's packet,
broker, ordinary UDP, and native API tests run without installing a driver.
Live adapter creation/configuration is pending validation. See the
[Windows network adapter guide](docs/windows-network-adapter.md) for the
architecture, test results, setup commands, and Python UDP example.

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

Vendored dependencies retain their own licenses. In particular, the bundled
libusb source is covered by [LGPL 2.1](vendor/libusb1-sys/libusb/COPYING).

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
