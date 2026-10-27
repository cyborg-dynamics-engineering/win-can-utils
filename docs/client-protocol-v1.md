# Windows CAN interface service, protocol v1

`canserver` runs one named interface per process. The broker owns the hardware;
applications connect independently. It is a user-space interface, not an NDIS
network adapter. It currently runs in the foreground; installing an SCM service
or arranging startup at login is outside this implementation.

## Configure once, connect many times

```powershell
# Save settings without opening the adapter. Prefer a serial number to a USB address.
canctl can0 create --driver gsusb --device SERIAL/0 --bitrate 500000

# Keep this process running. A newly created configuration starts down.
canserver --name can0

# From other terminals:
canctl can0 up
candump -d can0
candump "can0,123:800007FF" --queue-capacity 4096
cansend can0 123#010203
canctl can0 show
canctl can0 clients
canctl can0 down
canctl can0 set --bitrate 250000 --timestamps false
canctl can0 up
```

The existing creation command also works and saves its settings:

```powershell
canserver gsusb --device SERIAL/0 --name can0 --bitrate 500000
# On subsequent runs:
canserver --name can0
```

`canserver virtual --name vcan0` supplies a hardware-free local CAN interface.
Transmissions use the same local echo rules as physical backends. For a single
command to display its own echo:

```powershell
cansend vcan0 123#010203 --receive-own-messages
```

The default configuration location is
`%LOCALAPPDATA%\win-can-utils\interfaces\NAME.json`. Pass `--config PATH` to
`canctl` or `canserver` to use another file. Online `canctl` operations address
the running interface by name; the broker writes its own configuration path.
`canctl NAME show --config PATH` can display saved settings when the broker is
stopped. `create` refuses to overwrite a configuration or change a live interface.

```json
{
  "version": 1,
  "name": "can0",
  "driver": "gsusb",
  "device": "SERIAL/0",
  "bitrate": 500000,
  "timestamps": true,
  "up": false
}
```

Drivers are `gsusb`, `slcan`, `pcan`, and `virtual`. SLCAN devices use a serial
port such as `COM5`; PCAN uses a selector such as `USBBUS1`. New configs require
a positive bitrate; hardware-specific validation happens when brought up.
Names contain 1–64 ASCII letters, digits, underscores or hyphens. Pipe names
and reservations are case insensitive. Settings are written to a synced temporary
file beside the destination, then atomically replaced. Failed saves leave the
previous live settings unchanged.

`set_up` saves the desired state before opening or closing hardware. A failed
open leaves `desired_up=true` and an operational state of `faulted` or
`disconnected`. `up` retries with a fresh backend. There is no automatic reconnect
or automatic bus-off restart. `down` releases the backend; clients and queues
remain connected. A failed close reports `faulted` and its error. Previously
queued traffic can still arrive after `down`. Bitrate and timestamp settings
can be changed only while hardware is closed. Ctrl+C closes hardware without
changing the saved desired state, which is restored at the next start.

## Transport and framing

Connect to the local, duplex, byte-mode Windows named pipe:

```text
\\.\pipe\wincan_NAME_v1
```

Remote pipe clients are rejected. Windows pipe ACLs govern access; the protocol
does not add authentication or separate observer and administrator permissions.
Any client admitted to the duplex pipe can send or configure the interface.

Each message in either direction is a **four-byte unsigned little-endian byte
length**, followed by that many **UTF-8 JSON bytes**, without a newline.
Lengths must be 1–65,536 inclusive. Handle partial reads/writes and several
messages in one read. Zero, excessive, truncated, or malformed messages close
that connection. Unsupported versions receive `invalid_version` and disconnect.
The first message must be `hello` within ten seconds.

Every request has integer `version:1`, integer `id` (unsigned 64-bit correlation
ID), and `op`. IDs must be unique among that client's outstanding requests.
Integers in this protocol are JSON integers; bindings should preserve their
full width. Commands execute in order on each connection. Replies and frame
events share the pipe and may be interleaved. Duplicate IDs do not deduplicate
commands. Do not retry a send after an ambiguous disconnect or timeout unless
duplicate transmissions are acceptable.

```json
{"version":1,"id":1,"op":"hello","subscription":{"queue_capacity":1024}}
```

Reply shape (the `status` object is described below):

```json
{"version":1,"kind":"reply","id":1,"reply":{"result":"hello","client_id":7,"status":{}}}
```

The `{}` above is a placeholder. Actual replies contain the full status object.
`hello` may occur only once per connection. Client IDs are allocated per broker
process and are shared with legacy connections. The maximum is 128 simultaneous
data clients per interface, including legacy read/write connections.

## Commands

The following are complete request bodies; transport framing is omitted.

```json
{"version":1,"id":2,"op":"status"}
{"version":1,"id":3,"op":"clients"}
{"version":1,"id":4,"op":"set_up","up":false}
{"version":1,"id":5,"op":"configure","bitrate":250000}
{"version":1,"id":6,"op":"configure","timestamps":false}
{"version":1,"id":7,"op":"set_up","up":true}
{"version":1,"id":8,"op":"send","frame":{"id":291,"dlc":3,"data":[1,2,3]}}
{"version":1,"id":9,"op":"subscribe","subscription":{"queue_capacity":1024,"receive_own_messages":true}}
```

`configure` requires at least one of `bitrate` and `timestamps`, and preserves
omitted fields. `subscribe` replaces subscription options, with defaults for
omitted fields. Queue capacity cannot change on an existing connection; reconnect
to resize it. Already queued frames retain the previous filtering decisions.

`send`, `configure`, and `subscribe` return `{"result":"ok"}` inside `reply`.
`status` and `set_up` return `{"result":"status","status":{...}}`.
`clients` returns `{"result":"clients","clients":[...]}`.

Errors are correlated replies; valid command errors keep the connection open:

```json
{"version":1,"kind":"error","id":8,"code":"interface_down","message":"Interface is not up"}
```

Codes include `invalid_request`, `invalid_version`, `interface_down`,
`interface_up`, `configuration_error`, `driver_error`, and `unavailable`. The message supplies details.
Client-side response timeouts do not undo a command. Invalid JSON/schema closes
the pipe since no trustworthy request ID may be available.

## Subscriptions, queues, and filters

| Subscription field | Default | Meaning |
| --- | --- | --- |
| `receive` | `true` | Enable frame delivery. Use false for control/transmit-only clients. |
| `queue_capacity` | `1024` | Bounded server queue, 1–65,536 frames. |
| `filters` | `[]` | Up to 256 mask filters; empty accepts all ordinary frames. |
| `join_filters` | `false` | OR filters by default; true uses AND. |
| `error_mask` | `0` | Error-class mask; errors are excluded by default. |
| `receive_own_messages` | `false` | Also deliver this connection's local transmissions. |

Filters have `id`, `mask`, and optional `invert:false`. Match when
`(raw_id & mask) == (id & mask)`; invert reverses the comparison. `raw_id`
combines the arbitration ID with `0x80000000` for extended, `0x40000000` for
RTR, and `0x20000000` for errors. Error frames use `error_mask` independently
of ordinary ID filters: `(error_id & error_mask) != 0`. Set `receive=false`
to accept no events at all. `candump` accepts `id:mask`, `id~mask`, `j`/`J`,
and `#error_mask` after the interface name, separated by commas.

The broker attempts a nonblocking enqueue to every matching subscriber, in
receive/admission order. When one queue is full, it **drops that subscriber's
new frame** and increments its counter. Other subscribers continue. Old frames
stay in order. There is no replay for new subscribers, and no central backlog
when nobody is listening. Sequence numbers are global per interface process;
gaps may also be caused by filters or suppressed own transmissions.

`clients` reports active clients with `client_id`, `queue_capacity`, `queued`,
`delivered`, `dropped`, and `filtered`. `queued` excludes one possible in-flight
write and Windows' own pipe buffer. `delivered` counts completed pipe writes,
not application consumption. `filtered` includes disabled receive and suppressed
own transmissions. These counters last until disconnect. Interface
`counters.client_drops` retains the sum of server queue drops across disconnected
clients until the broker process restarts.

The Rust client also has a bounded local event queue, drained independently of
control replies. Its `local_drops()` counts additional drops there.
`candump -d` polls its server counter and its local counter once a second and
reports changes to stderr. A reader with an old SDK cannot query its own drops;
`canctl NAME clients` can inspect its server-side counters.

## Frames and transmit echo

`frame` has `id`, `dlc`, `data` (an array of byte integers), and optional booleans
`extended`, `rtr`, and `error`, all false by default. Standard IDs are at most
`0x7ff`; extended/error IDs are at most `0x1fffffff`. DLC is 0–8. Ordinary data
length equals DLC. RTR data must be empty, though DLC may be nonzero. Error
frames carry an error-class ID mask and no payload/DLC. Error transmission is
rejected. Unknown frame fields, including FD flags, are rejected.

```json
{
  "version":1,
  "kind":"frame",
  "event":{
    "sequence":42,
    "frame":{"id":291,"extended":false,"rtr":false,"error":false,"dlc":3,"data":[1,2,3]},
    "direction":"local_tx",
    "source_client_id":7,
    "timestamp_us":1790000000000000,
    "hardware_timestamp_us":null
  }
}
```

On successful backend admission, `send` increments `tx_frames` and publishes
one `local_tx` event to matching subscribers. The sender is excluded unless
its `receive_own_messages` is true. Other clients need no special echo option.
Echo obeys filters, queue limits, and drop accounting. `timestamp_us` is host
UNIX time in microseconds at admission; it is **not a CAN acknowledgement or
confirmed wire-transmit time**. A later asynchronous driver failure is reported
in interface status; it cannot retract an already accepted send or echo.

Hardware firmware echoes used internally by gs_usb are consumed by that driver
and are not published again. A separately received bus frame is still an `rx`
event even if its ID/data equal an earlier transmission; payload comparison
does not deduplicate frames.

Physical receive events have `direction:"rx"`, `source_client_id:null`, host
UNIX `timestamp_us`, and optional `hardware_timestamp_us`. The latter is a
backend-relative microsecond clock, potentially reset by reopen; it is not
UNIX time. Software TX always has a null hardware timestamp. Disabling timestamp
capture clears hardware timestamps while retaining host timestamps.

## Status and capabilities

`status` contains:

- Identity and configuration: `name`, `driver`, `device`, `bitrate`,
  `desired_up`, `timestamps` (requested capture).
- `state`: `down`, `up`, `disconnected`, or `faulted`.
- `bus_state`: `unknown`, `error_active`, `error_warning`, `error_passive`,
  `bus_off`, or `stopped`. An up interface may be bus-off.
- `last_error` (nullable), `active_clients`, `last_rx_timestamp_us` (nullable
  host UNIX time), `hardware_rx_error_counter` and `hardware_tx_error_counter`
  (nullable controller values).
- `counters`: `rx_frames`, `tx_frames` (accepted sends), `rx_errors` (error
  frames and backend receive/health failures), `tx_errors` (failed backend
  sends), `client_drops`, `disconnects`, and `bus_off_events` (observed entries).
  Rejected malformed/down-interface requests are not backend transmit attempts.
  Counters survive down/up and reset when the broker process restarts.
- `capabilities`: `hardware_can_fd` (nullable bool), `can_fd`,
  `hardware_timestamps` (nullable bool), `bus_state` (polling supported), and
  `software_tx_echo`.

Capabilities are discovered on first successful open and retained while down.
`null`/`unknown` means unavailable, not false or zero. State is polled about once
a second while up; hardware receive error frames can also report bus-off/restart.
Health information is cached for status requests. There are no unsolicited
status-change events in v1.

gs_usb reports its feature flags and uses `GET_STATE` when advertised, including
controller RX/TX error counters. PCAN uses `CAN_GetStatus` and always supplies
receive timestamps; classic PCAN-Basic cannot identify FD hardware capability
through this backend. SLCAN cannot reliably query bus state, FD hardware, or
timestamp capability and reports those as unavailable. Its receive wait is bounded so
an idle serial adapter does not block controls. The virtual backend reports
an active bus without hardware timestamps or CAN FD.

Protocol v1 and `CanFrame` carry classic CAN only. An FD-capable adapter may
report `hardware_can_fd:true` while `can_fd:false`. FD mode and 64-byte payloads
require a future end-to-end implementation.

## Rust and Python clients

The Cargo manifest pins upstream `crosscan` revision `0d65f3f` (version 0.2.0).
Its portable `protocol` module defines the wire types and framing; on Windows,
`service::{ServiceClient, ServiceControl}` supplies the async duplex client.
See the [cross-can source](https://github.com/Cyborg-Dynamics-Engineering/cross-can/tree/0d65f3ff2d3bf5bdb19b8e92826c78c223099e64).
The existing async `CanInterface` and legacy `WindowsCan` APIs remain available.

```rust,no_run
use crosscan::{
    protocol::{Command, Frame, Reply, Subscription},
    service::ServiceClient,
};

# async fn example() -> std::io::Result<()> {
let mut bus = ServiceClient::connect("can0", Subscription {
    receive_own_messages: true,
    ..Default::default()
}).await?;
let control = bus.control(); // Cloneable; usable concurrently with next_event().
control.request(Command::Send {
    frame: Frame { id: 0x123, dlc: 1, data: vec![1],
        extended: false, rtr: false, error: false },
}).await?;
let echo = bus.next_event().await?;
if let Reply::Status { status } = control.request(Command::Status).await? {
    println!("{:?}, drops={}", status.state, status.counters.client_drops);
}
println!("{:?}; local drops={}", echo, bus.local_drops());
# Ok(()) }
```

The SDK routes replies separately from event consumption, caps outstanding
requests at 128, and gives requests a 15-second deadline. Opening retries
missing/busy pipes for up to three seconds. Dropping the client closes its
transport and stops its IO task. `next_event()` is safe to cancel while waiting.

`examples/service_client.py` demonstrates framing and a control/transmit-only
client using Python's standard library. Other bindings need only local named
pipe IO, little-endian integers, and JSON.

## Legacy endpoints

Existing `crosscan::win_can::WindowsCan` clients remain supported:

- `\\.\pipe\can_NAME_in`: bincode frame followed by one newline byte.
- `\\.\pipe\can_NAME_out`: one-byte bincode length followed by a frame.
- `\\.\pipe\can_NAME_config_out`: JSON bitrate/package-version snapshot,
  terminated by EOF.

There are multiple instances of each endpoint. Legacy consumers get independent
1024-frame queues, all error classes, and local echoes from transmit connections.
Input/output connections have separate client IDs, so legacy duplex clients
cannot suppress their own TX echoes or set per-client filters. The v1 endpoint
supplies those options, acknowledgements, counters, and lifecycle controls.
As with Windows named pipes generally, old synchronous `WindowsCan::open*`
callers should retry `ERROR_PIPE_BUSY` (231) during simultaneous connections.
No legacy endpoint provides replay or global receive backpressure.

## Verification

Run `cargo test --all-targets --offline`. Tests use actual Windows pipes and
mock/virtual drivers to cover fan-out, queue overflow isolation, legacy clients,
framing, filters, echo, persistence, lifecycle, bus-off and disconnect recovery.
The existing opt-in gs_usb hardware test remains separate and ignored unless
explicitly selected with isolated adapters configured.
