# Windows CAN Utils

Windows CAN Utils provides a Windows equivalent of [linux-can/can-utils](https://github.com/linux-can/can-utils). It offers command-line tools to interact with CAN devices via USB-to-CAN transceivers.

Currently supported drivers:  
- **canable slcan adapters** (e.g., DSD Tech [SH-C30A](https://www.deshide.com/product-details_SH-C30A.html), [SH-C31A](https://www.deshide.com/product-details_SH-C31A.html))  
- **PEAK-System PCAN adapters** (requires [PCAN-Basic](https://www.peak-system.com/PCAN-Basic.239.0.html?&L=1))  

---

## Quick Start

1. **Install**:  
   - Download the latest `.msi` from [Releases](https://github.com/Cyborg-Dynamics-Engineering/win-can-utils/releases).  
   - Run the installer.  
   - (Optional) For PEAK adapters, install [PCAN-Basic](https://www.peak-system.com/PCAN-Basic.239.0.html?&L=1).  

2. **Connect your adapter**:  
   - For slcan → check the COM port in **Device Manager → Ports (COM & LPT)**.  
   - For PCAN → ensure PEAK drivers are installed.  

3. **Start a CAN server**:  
   ```sh
   canserver slcan COM5 --bitrate 1000000
   ```

4. **Dump CAN frames**:  
   ```sh
   candump COM5
   ```

5. **Send a CAN frame**:  
   ```sh
   cansend COM5 055#00
   ```

You’re now online with Windows CAN Utils!

---

## 📖 Full Documentation

### Installation
1. Download and install the `.msi` from [Releases](https://github.com/Cyborg-Dynamics-Engineering/win-can-utils/releases).  
2. For PEAK adapters, see [PCAN-Basic dependency](#pcan-basic-dependency).  
3. Run commands from any terminal.  

> **Tip:** Use administrator privileges when installing drivers.

---

### PCAN-Basic Dependency

Required **only** for PEAK-System adapters.  

1. Download [PCAN-Basic](https://www.peak-system.com/PCAN-Basic.239.0.html?&L=1).  
2. Accept the EULA (use is restricted to PEAK hardware).  
3. Copy `PCANBasic.dll` into either:  
   - the same folder as the executables, or  
   - a directory listed in your system `PATH`.  
4. Restart Windows CAN Utils processes.  

> **Disclaimer:** Windows CAN Utils is not affiliated with PEAK-System. See PEAK’s EULA for permitted usage.

---

### Usage

#### CAN Server
Opens a CAN connection and exposes it as a Windows [pipe](https://learn.microsoft.com/en-us/windows/win32/ipc/pipes).  
Check **Device Manager → Ports (COM & LPT)** for available adapters.

```
Usage: canserver <driver> <port> [--bitrate <bitrate>]
Example: canserver slcan COM5 --bitrate 1000000
```

#### CAN Dump
Displays CAN frames in real time.  
```
Usage: candump <port>
Example: candump COM5
```

#### CAN Send
Sends a single CAN frame.  
```
Usage: cansend <port> <ID#DATA>
Example: cansend COM5 055#00
```

> Note: `candump` and `cansend` require a running `canserver`.

---

### slcan Firmware Installation

Some adapters may not ship with slcan firmware. Flash them with the [CANable updater](https://canable.io/updater/):  

- [SH-C30A firmware](https://canable.io/updater/canable1.html)  
- [SH-C31A firmware](https://canable.io/updater/canable2.html)  

**Steps**:  
1. Disconnect the adapter.  
2. Set the boot switch to **On**.  
   <br><img src="https://github.com/user-attachments/assets/154c4837-61d0-402f-9a38-76f50d5a5f81" width="200">  
3. Reconnect → run updater → flash firmware.  
4. Switch back to **Off**, reconnect, and test with `canserver`.  

---

### License
Windows CAN Utils is dual-licensed under:  
- [Apache 2.0](http://www.apache.org/licenses/LICENSE-2.0)  
- [MIT](http://opensource.org/licenses/MIT)  

---

## Developer Guide

### Contribution
Contributions are welcome! Unless stated otherwise, submissions are dual-licensed Apache/MIT.

### Build from Source
```sh
cd win_can_utils
cargo install --path .
```

### Generate MSI Installer
1. Install [cargo-wix](https://github.com/volks73/cargo-wix):  
   ```sh
   cargo install cargo-wix
   ```
2. Download [WiX3 binaries](https://github.com/wixtoolset/wix3/releases).  
   Add `bin/` to your PATH.  
3. Run:  
   ```sh
   cargo wix
   ```
