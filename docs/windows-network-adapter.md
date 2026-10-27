# Experimental Windows network adapter

`canadapter` bridges a **real Wintun NDIS network adapter** to the existing CAN
interface service. Windows applications use ordinary IPv4 UDP sockets to speak
CAN service v1 JSON through that adapter. The physical CAN device remains owned
by `canserver`; existing named-pipe clients work alongside UDP applications.

This is a CAN-over-UDP adapter prototype. It does not implement Winsock `AF_CAN`,
a Windows CAN network-device class, or Linux SocketCAN binary compatibility.
The Windows adapter's nominal link speed is unrelated to the configured CAN
bitrate. Query the CAN service for bitrate and controller health.

```text
Application UDP socket (198.18.0.1:ephemeral)
       | JSON datagrams to 198.18.0.2:29536
Windows IP stack / dedicated Wintun NDIS adapter
       | layer-3 packets through the public Wintun API
canadapter.exe
       | one independent CAN service connection per UDP source port
canserver.exe -> gsusb / PCAN / SLCAN / virtual backend
```

## Current validation and permission boundary

The prototype compiles with the existing Windows SDK and Rust toolchain. The
installed Wintun DLL was Authenticode-verified as signed by WireGuard LLC. A
read-only probe successfully loaded all required API exports and reported a
running driver version of `0.14`, with `elevated:false`.

Tests exercise IPv4/UDP checksums, malformed packets, maximum payloads, ICMP
echo, actual CAN named pipes, subscriber filters/echo, state changes, idle
expiry, ring pressure, cancellation, and adapter removal. An ordinary UDP socket
and the Python example also run through the complete bridge with a simulated
TUN/IP boundary. A separately compiled user-mode test DLL verifies the actual
runtime FFI, packet allocation/release, event waits, and session/adapter cleanup.

**Live creation of a new adapter and configuration of its IP address have not
been validated.** The process is not elevated; the creation preflight returns
`PermissionDenied` without requesting elevation or changing network settings.
Those checks require a later elevated run. No driver was installed, no existing
adapter was opened for a packet session, and no boot-signing policy was changed.

The custom kernel-driver route was also investigated. This machine has Windows
SDK 10.0.26100.0 but lacks WDK kernel headers (`ndis.h`, `ntddk.h`) and libraries.
An unprivileged attempt to retrieve NuGet package metadata was blocked by the
execution sandbox. The Wintun provider avoids that build dependency by using
the public API of an existing signed NDIS miniport.

## Build and read-only diagnostics

```powershell
cargo build --bins --locked --offline
.\target\debug\canadapter.exe probe --dll "C:\Program Files\Tailscale\wintun.dll"
```

The DLL path is explicit and absolute. Dependencies are loaded only from its
directory or System32. The probe does not create an adapter or open a packet
session. An installed DLL is optional; an official, architecture-matching
Wintun distribution can be obtained from [wintun.net](https://www.wintun.net/).
This repository does not redistribute Wintun binaries or source. The official
binary archive includes its own redistribution license.

## Live test when administrator access is available

Start with the virtual CAN backend, so the networking test needs no external
CAN bus. In a normal terminal in this repository:

```powershell
.\target\debug\canserver.exe virtual --name vcan0 --config .\target\vcan0-network.json
```

In an **elevated terminal**, run the bridge with a new dedicated adapter name:

```powershell
.\target\debug\canadapter.exe run --interface vcan0 --adapter "WinCAN vcan0" `
  --dll "C:\Program Files\Tailscale\wintun.dll" --create --configure-ip `
  --stats-file .\target\network-adapter-stats.json
```

Alternatively, `scripts/start-network-adapter.ps1 -Dll PATH -Interface vcan0`
supplies those flags. The script requires an already elevated shell and never
opens a UAC prompt itself.

The default address plan is `198.18.0.1/30`, with synthetic peer `198.18.0.2`
and UDP port `29536`. Only the local address is assigned to Windows; the bridge
implements the peer. These are benchmark/test addresses, not an Internet
gateway. `--local`, `--peer`, `--prefix-length`, and `--port` override them.
The address preflight rejects overlap with existing interface subnets.

Creation/configuration affects only the newly created dedicated adapter. It
adds its IPv4 address and connected subnet route, sets MTU 65535, and disables
IPv4 forwarding on that adapter. It does not configure a default route, DNS,
NAT, firewall exemptions, or driver-signing settings. Creation refuses an
already existing adapter name. The normal run mode can open an explicitly
named preconfigured adapter; IP configuration is restricted to adapters created
by this process. Use a dedicated WinCAN adapter for the bridge.

In another normal terminal:

```powershell
Get-NetAdapter -Name "WinCAN vcan0"
Get-NetIPAddress -InterfaceAlias "WinCAN vcan0" -AddressFamily IPv4
ping 198.18.0.2
python examples/network_can.py --status
python examples/network_can.py --send 123#010203 --receive-own-messages
```

Run a simultaneous listener:

```powershell
python examples/network_can.py --dump --filter 123:800007FF
```

The listener and `candump vcan0` should both see the transmission. The sending
UDP client also receives its own echo when requested. Echo still means backend
admission, not a CAN acknowledgement. For hardware, run a configured `canserver`
interface and pass its name to `--interface`.

Ctrl+C stops the bridge's sessions and reader, ends the Wintun session, and
removes an adapter created by this process. The installed shared Wintun driver
remains installed. Opening an existing adapter does not transfer its ownership
to this process; closing its handle leaves that adapter installed.

## UDP transport

The JSON schema is the existing [CAN service v1 protocol](client-protocol-v1.md).
Each UDP payload is **one JSON message without a length prefix or newline**.
Each source port has its own broker connection, filters, bounded event queue,
client ID, counters, and echo options. The destination IP/port is the configured
synthetic peer. Applications should bind to the configured local IP.

```json
{"version":1,"id":1,"op":"hello","subscription":{"receive_own_messages":true}}
{"version":1,"id":2,"op":"send","frame":{"id":291,"dlc":3,"data":[1,2,3]}}
{"version":1,"id":3,"op":"status"}
```

Replies use the same `reply`, `error`, and unsolicited `frame` envelopes as the
named-pipe service. A valid first request must be `hello`. `hello_required`
indicates a new or expired endpoint; `client_limit` means the UDP session limit
was reached. Each UDP endpoint consumes one of the broker's client slots.

The default limit is 32 UDP clients (`--max-clients`, maximum 128). Sessions
expire after 60 seconds without a request (`--idle-seconds`, 1–86400). Passive
listeners must send a keepalive such as `status`; the Python example does so
every 20 seconds. Frames alone do not renew the lease. Malformed JSON or an
unsupported version retires that endpoint. Valid command errors preserve it.

UDP has no reliable delivery, ordering or duplicate suppression. Responses may
be lost after a CAN send has already been accepted. **Do not automatically retry
send requests after a timeout.** The Python client sends each request once.
Frames and replies can interleave and must be distinguished by `kind` and `id`.

Maximum UDP payload is 65507 bytes. IPv4 header and UDP checksums are verified
(an absent UDP checksum is valid for IPv4). Outgoing checksums are generated in
software. IP options are skipped using IHL. IP fragments, IPv6, TCP, unrelated
destination ports and foreign source/destination addresses are not forwarded.
MTU 65535 avoids fragmenting valid v1 requests on the dedicated link. The only
other implemented network protocol is ICMP echo for ping diagnostics.

The endpoint is local to the dedicated adapter and has no authentication layer.
Any local application able to use it has the full v1 control API, including CAN
transmission, up/down, and configuration. The gateway does not create a listener
on physical LAN interfaces or provide an IP forwarding service.

## Queues, loss, and cleanup

The packet ingress and egress queues each hold 1024 packets. Each endpoint's
command queue holds 16 requests. Broker subscriptions retain their own bounded
queues. Full packet rings/queues increment counters and drop packets rather than
stalling CAN reception or other sessions. A failed adapter receive/send stops
the bridge; ring-full is treated as a recoverable drop.

On Ctrl+C, a JSON report is printed and optionally saved with `--stats-file`:
`packets_from_windows`, `packets_to_windows`, `invalid_packets`, `ignored_packets`,
`ingress_drops`, `egress_drops`, `ring_drops`, `protocol_errors`,
`session_queue_drops`, `sessions_created`, `sessions_expired`, `active_clients`,
`broker_local_drops`, `accepted_sends`, and `icmp_replies`.

Broker-side subscription drops remain available through `clients`/`canctl`.
`broker_local_drops` counts additional drops in the bridge's Rust SDK event
queues. UDP socket receive-buffer drops inside the Windows stack are outside
the bridge's counters. `icmp_replies` counts queued replies, which may themselves
be dropped by egress/ring pressure. The process never calls `WintunDeleteDriver`.

## Reproduce automated verification

```powershell
$env:WINCAN_TEST_PYTHON = (Get-Command python).Source
cargo test --all-targets --locked --offline
```

The environment variable additionally runs `examples/network_can.py` in the
ordinary UDP loopback integration test. That test simulates only the TUN/IP
boundary and does not create a Windows adapter. The native FFI test compiles
`tests/support/wintun-test-double.c` with MSVC and the installed Windows SDK into
`target/`, then checks runtime binding, packet ownership, and RAII cleanup.
This DLL is only a user-mode test double; it installs no driver. The existing
physical gs_usb hardware test remains opt-in.

Relevant upstream references:

- [Wintun project and signed distributions](https://www.wintun.net/)
- [Wintun public API and lifecycle](https://git.zx2c4.com/wintun/tree/README.md)
- [Microsoft NDIS virtual miniport sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/ndis-virtual-miniport-driver/)
- [Microsoft WDK NuGet guidance](https://learn.microsoft.com/en-us/windows-hardware/drivers/install-the-wdk-using-nuget)
