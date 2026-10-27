# gs_usb receive validation

The CPU problem was above the USB transport: `GsUsbDriver::read_frames` drained
its channel with `try_recv` and immediately returned an empty vector. `canserver`
called it again without waiting, repeatedly acquiring the driver mutex. Adding a
sleep there would delay every empty read; waiting on the frame channel while
holding the driver mutex would prevent transmission on an idle bus.

The driver now exposes an optional receive notification. The server drains the
queue, releases the driver mutex, and waits only when the batch was empty.
Notifications retain a permit across the gap between reading and waiting. The
USB worker notifies after enqueueing frames and after its sender closes.

64 USB reads remain queued with no transfer timeout. Submission is eager,
completed reads are consumed in submission order, and replacement reads are
submitted before parsing. This avoids depending on future wake order for byte
stream assembly. The libusb event thread blocks on OS completion events with a
one-second maximum wait; completion wakes it immediately. Shutdown interrupts
that wait and cancels/drains outstanding reads before closing their handle.
This follows libusb's [event handling thread model](https://libusb.sourceforge.io/api-1.0/group__libusb__poll.html)
for asynchronous transfers on Windows.

The server also used to ignore `try_send` errors on its 100-message IPC queue.
A scheduling delay could therefore discard frames after successful USB reception.
It now waits for capacity outside the driver lock. While its decoded-frame queue
is full, the USB worker continues servicing transmit and control commands.
Buffers remain bounded; sustained consumption below the incoming rate will
eventually overflow the physical adapter. Reception is a live stream, not an
unlimited recording service for disconnected readers.

## Automated checks

Run `cargo test --offline` and `cargo fmt -- --check`.

The server tests cover idle reads without repeated polling, transmit while idle,
a notification issued between an empty read and waiting, and exact delivery of
4,096 frames through a one-message output queue, including timestamps and order.
These tests simulate incoming CAN frames; they do not establish hardware loss or
CPU measurements. Callback tests also check received byte counts, cancellation
and disconnection wakeups, and stopping an idle libusb event thread.

## SH-C31A idle results, 27 October 2026

Tested the connected SH-C31A, which reports the USB product string `SH-C31x`,
with nothing connected to its CAN bus. Both release builds used 1 Mbit/s, logging
set to `error` during sampling, and the same machine with eight logical CPUs.
Each condition was measured for 30 seconds using Windows `GetProcessTimes`.

| Receive implementation | IPC reader | CPU seconds | CPU as one core | CPU across machine |
| --- | --- | ---: | ---: | ---: |
| Original | Absent | 35.844 | 119.33% | 14.92% |
| Original | Connected | 35.672 | 118.83% | 14.85% |
| Modified | Absent | 0 measured | 0.00% measured | 0.00% measured |
| Modified | Connected | 0 measured | 0.00% measured | 0.00% measured |

Here 100% of one core means one CPU second per wall-clock second. Zero measured
means that the process CPU counter did not advance over the sample; it is not a
claim that absolutely no housekeeping runs. Full measurements are recorded in
[the results JSON](gs-usb-idle-results-2026-10-27.json).

The original receive implementation came from revision `b7850b1`. Both benchmark
builds needed the same compatibility corrections before they could initialize
this adapter:

- The device uses WinUSB on interface 0 and libusbK on interface 1. The bundled
  libusb 1.0.27 opens all WinUSB-like interfaces for each backend, causing a
  second open of the exclusive WinUSB interface and `LIBUSB_ERROR_ACCESS`.
  A direct Windows read/write handle probe succeeded. Filtering open/close by
  the interface's backend let both test builds open the adapter.
- This firmware rejected truncated descriptor requests. `DEVICE_CONFIG` needs
  12 bytes, and `BT_CONST_EXT` needs 72 bytes. The feature mask is already present
  in the full timing descriptor, so a separate four-byte query is unnecessary.
  The lengths match the structures in the [Linux gs_usb driver](https://github.com/torvalds/linux/blob/master/drivers/net/can/usb/gs_usb.c).

The idle benchmark used an isolated dependency correction, with its historical
[patch saved here](libusb-1.0.27-mixed-backends.patch). That executable is
`target/gsusb-benchmark/canserver-modified-usbfix.exe`. The normal Cargo build now
includes a broader correction in [vendored libusb1-sys](../vendor/libusb1-sys/LOCAL_CHANGES.md):
it opens an interface when claimed and closes only handles belonging to that
interface's backend. Both connected adapters now work with the ordinary release
build. Windows driver bindings were not changed.

Five open/start/reset/drop cycles also passed in one test process, checking that
pending reads release the device before a subsequent open. The hidden-console
benchmark harness could not stop either server using Ctrl+C, so it terminated
them after sampling; interactive Ctrl+C remains a manual check. No test server
was left running.

These initial measurements verify idle CPU use and reopening. Subsequent tests
with a connected CAN peer are described below.

## Multiple adapter validation, 27 October 2026

The ordinary release build opened both connected adapters in separate processes:

| USB interface at test time | Product | Serial | Windows backend |
| --- | --- | --- | --- |
| `003:014/0` | SH-C31x (SH-C31A) | `0021002D3133500F20313355` | WinUSB |
| `003:015/0` | canable gs_usb | `002F00165046570120333836` | libusbK |

Discovery showed both initially available, then the first busy while the second
remained available, then both busy. Opening the second adapter while the first
was running succeeded. Automatic IPC reservations assigned `can0` and `can1`.
Duplicate adapter and duplicate IPC-name attempts were rejected. Both processes
recorded zero additional CPU seconds over a concurrent ten-second idle sample.
After cleanup, both interfaces were available again. A real terminal also showed
the selection list, rejected an invalid number, and opened the selected adapter.
The final concurrency/idle recheck is saved in
[the multiple-adapter results JSON](gs-usb-multiple-adapters-results-2026-10-27.json).

Discovery probes only the selected interface's claimability; it does not change
CAN bitrate or mode. Availability is a snapshot, and the final claim can fail if
another process opens an adapter after discovery. Busy interfaces may have only
a generic label because USB product/serial strings cannot always be read while
another process owns the interface. Their location ID and unavailable reason
remain visible.

## Load-test corrections

The hardware test exercises the complete path: client input pipe, server,
transmitting adapter, terminated CAN bus, receiving adapter, server output pipe,
and client decoder. Sequence numbers and their bitwise inverse fill each
eight-byte standard CAN frame, using identifier `0x5a0`.

Testing exposed additional problems that idle tests could not reveal:

- Input pipe reads can split or combine binary messages. The old reader decoded
  only the first frame in each chunk. A streaming decoder now retains partial
  messages and decodes every complete frame, including binary newline bytes.
- Transmit packet padding caused the SH-C31A to stall. The old recovery reset
  its CAN mode without restoring timestamp/padding flags, corrupting subsequent
  reception. Bulk OUT records now use their transport size without receive
  padding; endpoint recovery clears the halt without resetting CAN mode.
- SH-C31A receive padding expands classic frames to 128 bytes. At maximum
  CANable transmit rate, USB captures showed all transmit echoes but missing
  records from the SH-C31A, before host decoding. Compact receive records removed
  this bottleneck in the short verification test. Padding remains enabled on
  the classic-only CANable. The parser also handles a complete padded FD-capable
  transport record rather than treating its trailing zeros as an extra echo.
- Ten transmit echo slots now bound outstanding frames. Slots are reused only
  when firmware echoes them; queue exhaustion waits without polling and times
  out if no echoes arrive. Echoes return firmware credits; they are not an
  independent proof of CAN delivery. Some candleLight implementations echo when
  handing a frame to the CAN peripheral, before it succeeds on the bus (see the
  [upstream limitation](https://github.com/candle-usb/candleLight_fw#limitations)).
  The load test independently checks every frame at the receiving adapter.
  USB shutdown wakes waiting senders.
- Timestamp enablement uses the advertised feature and MODE START flag.
  BREQ_TIMESTAMP reads the device clock; it is not an OUT enable command.

Regression tests cover fragmented/coalesced pipe messages, split padded USB
records, incomplete payloads without timestamps, echo slot exhaustion/reuse,
and waking blocked transmitters on USB shutdown.

## Sustained load results, 27 October 2026

Each direction submitted continuously for 60 seconds, then drained pending
traffic. `loadA` is the SH-C31A and `loadB` is the CANable. The ordinary release
build was used, with logging at `error`, on the eight-logical-CPU machine.

| Bitrate (bit/s) | Direction | Frames received | Frames/s | Estimated bus load | SH-C31A server CPU (one core) | CANable server CPU (one core) |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1,000,000 | SH-C31A to CANable | 288,513 | 4,736 | 52.6–63.9% | 26.0% | 18.2% |
| 1,000,000 | CANable to SH-C31A | 443,488 | 7,320 | 81.3–98.8% | 24.7% | 35.8% |
| 125,000 | SH-C31A to CANable | 66,438 | 1,033 | 91.8–100% | 9.7% | 6.0% |
| 125,000 | CANable to SH-C31A | 66,478 | 1,036 | 92.0–100% | 6.1% | 9.6% |

All **864,917 submitted frames** arrived exactly once, in order, with valid
payloads. Missing frames, duplicates, corrupt payloads, unexpected/error frames,
timestamp regressions, and pipe errors were all zero. Server error logs were
empty. Every received frame in both 125 kbit/s runs had a hardware timestamp.
The 1 Mbit/s runs also checked timestamp order; their earlier test executable
did not report the timestamped-frame count separately.

The 1 Mbit/s tests ran unpaced at each adapter's achievable transmit rate. The
SH-C31A transmitter did not saturate that bitrate, which is why the 125 kbit/s
tests were added to exercise heavy bus load in both directions. Tests were
sequential, not simultaneous bidirectional traffic. They use classic CAN;
CAN FD and timestamp wrap after about 72 minutes remain untested in hardware.

CPU is Windows kernel plus user process time over the complete test, including
queue drain. Divide these one-core percentages by eight for whole-machine CPU
(the highest result, 35.8%, is about 4.5% of this machine). Raw counts, timing,
CPU counters, adapter identities and executable hashes are saved in
[the load results JSON](gs-usb-load-results-2026-10-27.json).

## Repeat and transmit timing, 27 October 2026

Repeated all four 60-second tests with the same release executables. The CANable
had been reconnected at `003:019/0`, with the same serial number, and now used
WinUSB rather than the first run's libusbK. The test harness did not change USB
driver bindings. Both adapters used WinUSB for this repeat.

| Bitrate (bit/s) | Direction | Frames received | Frames/s | Missing |
| ---: | --- | ---: | ---: | ---: |
| 1,000,000 | SH-C31A to CANable | 279,310 | 4,577 | 0 |
| 1,000,000 | CANable to SH-C31A | 430,500 | 7,108 | 0 |
| 125,000 | SH-C31A to CANable | 66,185 | 1,029 | 0 |
| 125,000 | CANable to SH-C31A | 66,622 | 1,036 | 0 |

All 842,617 frames arrived exactly once and in order, with valid payloads and
hardware timestamps. There were no unexpected/error frames, timestamp
regressions, pipe errors, or server error logs. The 1 Mbit/s rates were 3.4% and
2.9% below the first run. The 125 kbit/s runs estimated 91.3–100% and 92.0–100%
bus load. Raw measurements are in [the repeat JSON](gs-usb-repeat-results-2026-10-27.json).

To locate the remaining transmit difference, a separate temporary diagnostic
build added aggregate timers around the blocking USB write, echo-slot acquire,
and complete `send_frame` call. It changed no scheduling logic and logged only
once per 5,000 frames. A ten-second unpaced run in each direction again delivered
every frame. The final cumulative timing samples were:

| Measurement | SH-C31A | CANable |
| --- | ---: | ---: |
| Samples in final timing snapshot | 45,000 | 75,000 |
| USB OUT record size | 76 bytes | 20 bytes |
| Mean `send_frame` duration | 219.812 µs | 137.486 µs |
| Mean blocking USB write duration | 206.008 µs | 124.629 µs |
| Mean echo-slot acquisition duration | 0.472 µs | 0.432 µs |
| Other time within `send_frame` | 13.332 µs | 12.425 µs |

The synchronous USB write occupies 93.7% of SH-C31A send time. The driver waits
for one USB write to finish before beginning the next frame, so approximately
220 µs per call corresponds to about 4,550 frames/s, matching the diagnostic
result. Echo credit waiting is negligible. Almost all of the measured difference
between adapters is inside the blocking USB call. That call includes Windows USB
scheduling, backend/controller handling and device-side readiness; this test
does not separate those components or establish an intrinsic hardware limit.
The 76-byte SH-C31A record spans two USB packets on its 64-byte endpoint, while
the CANable's 20-byte record fits one packet on its 32-byte endpoint. This is a
plausible contributor, not an independently isolated cause.

This profiling motivated the asynchronous USB OUT queue described below. The
normal release binary was unchanged by profiling. Timings, diagnostic executable
hash and complete aggregate logs are in [the USB timing JSON](gs-usb-usb-timing-2026-10-27.json).
Both adapters were available after cleanup; the SH-C31A then enumerated at
`003:020/0`.

## Asynchronous transmit queue implementation

USB OUT transfers are now submitted eagerly, in command order, and completed
asynchronously alongside receive processing. Up to ten frames can be outstanding.
`send_frame` returns once libusb accepts the transfer; it does not wait for that
transfer's completion or establish CAN delivery. Each buffer, handle and context
stays alive until its completion callback or cancellation callback has retired.

A transmit credit is released only after **both** its USB completion and firmware
echo, regardless of arrival order. This prevents early echoes from reusing a buffer
slot while its USB transfer remains outstanding. Echo IDs advance across channel
resets so stale echoes cannot release new frames. Cancelling before submission
automatically returns the reserved credit. Control transfers drain earlier USB
writes; MODE RESET then discards outstanding echo credits from the old session.

Short writes, transfer timeouts and disconnects stop the worker, cancel other
pending transfers, and preserve the transport error for receive/subsequent send
operations. Partially written frames are not retried because retrying could
duplicate a frame already accepted by the adapter. The empty transmit queue
waits for activity without polling.

Automated checks include concurrent pending writes with callbacks arriving in
reverse order, draining before a control operation, both orders of echo/USB
completion, cancellation of unprocessed commands, stale echoes after reset,
partial writes, failures waking blocked senders, and real libusb write callbacks.
The opt-in hardware test in `tests/gsusb_hardware.rs` also passed six cycles of
queued transmission, close/reset/reopen, and dropping with writes outstanding.
Run it on an isolated, connected pair with:

```powershell
$env:GSUSB_TEST_A = '0021002D3133500F20313355/0'
$env:GSUSB_TEST_B = '002F00165046570120333836/0'
cargo test --test gsusb_hardware -- --ignored --nocapture
```

Use selectors from your own adapter list. The lifecycle check intentionally
cancels queued frames; it checks safe teardown rather than delivery. The separate
sequence-checked load test establishes end-to-end delivery.

### Asynchronous queue hardware results

The new queue passed 60 seconds in each direction at 1 Mbit/s and 125 kbit/s,
using the same DSD TECH v1.5 SH-C31A and classic CANable with WinUSB backends.
These runs finished before the SH-C31A was subsequently flashed with Cyder
firmware. All 950,007 submitted frames arrived exactly once and in order, with
valid payloads and timestamps, and no unexpected/error frames, timestamp
regressions, pipe errors, or server error logs.

| Bitrate (bit/s) | Direction | Received | Frames/s | Estimated bus load |
| ---: | --- | ---: | ---: | ---: |
| 1,000,000 | SH-C31A to CANable | 313,654 | 5,155 | 57.2–69.6% |
| 1,000,000 | CANable to SH-C31A | 503,061 | 8,315 | 92.3–100% |
| 125,000 | SH-C31A to CANable | 66,903 | 1,041 | 92.5–100% |
| 125,000 | CANable to SH-C31A | 66,389 | 1,033 | 91.8–100% |

Compared with the most recent synchronous repeat, SH-C31A transmit throughput
increased 12.6%, and CANable throughput increased 17.0%. The SH-C31A did not
saturate 1 Mbit/s; queueing transfers improved its rate but did not eliminate
all device/USB throughput limits. At 1 Mbit/s, the two servers used 32.9%/18.1%
of one core in the first direction and 29.6%/56.0% in reverse. At 125 kbit/s each
server used 6.1–11.9% of one core. These are process CPU measurements including
drain time, not whole-machine percentages.

After flashing Cyder firmware and installing WinUSB for its new identity
(`Cyder CANable gs_usb`, serial `205233823133`), initialization and channel open
at 1 Mbit/s also succeeded. This firmware reported features `0x2503` and a
170 MHz CAN clock. A ten-second idle sample measured zero additional process CPU
seconds. The single-adapter idle test sent no CAN frames; it does not validate
CAN delivery with the newly flashed firmware. The test server was stopped.

Raw results, executable hashes and CPU measurements are in
[the asynchronous transmit results JSON](gs-usb-async-transmit-results-2026-10-27.json).
The automated suite passed 25 tests; the opt-in hardware lifecycle test passed
separately.

### Cyder firmware hardware results

After reconnecting and terminating the CAN wiring, the SH-C31A running Cyder
firmware passed the same sustained tests against the classic CANable. Initial
testing exposed a host compatibility issue: advertising CAN FD support had
selected 76-byte transmit records even in classic CAN mode. Cyder expects
20-byte classic records. The host now uses that size by default, retaining the
76-byte quirk for the previously tested factory `SH-C31x` firmware. Two regression
tests cover this distinction.

Each direction submitted traffic for 60 seconds, followed by a drain period.
All **1,145,992 frames** arrived exactly once, in order, with valid payloads.
Missing, duplicate, reordered, corrupt, unexpected and error frames were all zero;
there were no pipe errors, timestamp regressions or server error logs.

| Bitrate (bit/s) | Direction | Received | Frames/s | Estimated bus load |
| ---: | --- | ---: | ---: | ---: |
| 1,000,000 | Cyder SH-C31A to CANable | 503,886 | 8,328 | 92.4–100% |
| 1,000,000 | CANable to Cyder SH-C31A | 508,661 | 8,405 | 93.3–100% |
| 125,000 | Cyder SH-C31A to CANable | 66,789 | 1,042 | 92.5–100% |
| 125,000 | CANable to Cyder SH-C31A | 66,656 | 1,038 | 92.2–100% |

Bus load is estimated from frame rate and possible bit stuffing, rather than
measured by an independent bus analyzer. These runs used standard eight-byte
classic CAN frames and sequential directions. The improvement over the earlier
factory firmware results includes both the firmware change and the corrected
classic transmit record size.

During the 1 Mbit/s runs, Cyder/CANable server CPU usage was 62.3%/36.4% of one
core when Cyder transmitted and 29.1%/50.6% in reverse. At 125 kbit/s each server
used 5.6–10.5% of one core. A separate ten-second concurrent idle sample measured
zero additional process CPU time for both servers. Both adapters could be opened
at once; listings showed busy adapters and rejected duplicate adapter or pipe
ownership.

Cyder advertises no hardware timestamp capability, so frames received through it
have no hardware timestamp. Frames received through the classic CANable did have
timestamps, with no regressions. CAN FD, simultaneous bidirectional load, and
timestamp wrap were not tested in these runs.

The automated suite passed 27 tests. The hardware lifecycle test also passed six
cycles of queued writes, reset/reopen, and immediate drop using the new firmware.
Both adapters were released after testing. Complete counts, CPU measurements,
firmware/executable hashes and adapter metadata are in
[the Cyder firmware results JSON](gs-usb-cyder-load-results-2026-10-27.json).

## Reproducing the hardware load test

Connect and terminate the adapters on an isolated CAN bus. Build with
`cargo build --release --bin canserver --example can_load_test`. Start one
server per adapter at the same bitrate, using the IDs from `--list-adapters`:

```powershell
canserver gsusb --device 003:014/0 --name loadA --bitrate 125000
canserver gsusb --device 003:015/0 --name loadB --bitrate 125000
```

In a third terminal, run each direction sequentially:

```powershell
target/release/examples/can_load_test.exe --source loadA --destination loadB --seconds 60 --bitrate 125000
target/release/examples/can_load_test.exe --source loadB --destination loadA --seconds 60 --bitrate 125000
```

The example prints JSON and exits unsuccessfully for missing, duplicate,
reordered, corrupt, unexpected or error frames, timestamp regressions, or pipe
errors. `--rate 0` (the default) sends as fast as backpressure permits. The
submitted count means frames accepted by the input pipe; equality with the
peer's checked receive count establishes end-to-end delivery. Allow queued
traffic to drain before judging loss. Server error logs must also be checked.

The reported load range uses 111–135 bits per standard eight-byte CAN frame,
including intermission and possible bit stuffing, and host-observed receive
timing. Its upper bound is capped at 100%. This is an estimate, not an independent
bus-analyzer measurement. The two adapters have different maximum transmit
rates; use a lower bitrate to test saturation in both directions.

## Hardware acceptance

1. Compare release builds on the same Windows machine, USB port, firmware, and
   CAN bitrate. Run `canserver gsusb --bitrate 1000000`, with logging at its
   default level. Attach a reader before starting the traffic generator.
2. With an idle bus, measure process CPU for at least 30 seconds, normalized by
   logical core count. Check both with and without a pipe reader. The server
   should no longer occupy one logical core just to wait for frames.
3. Use a separate CAN generator that embeds an incrementing sequence number in
   each payload. Capture and decode the pipe stream to a file without printing
   every frame. Compare against the generator's transmitted/acknowledged count;
   assert no missing or duplicate sequence numbers and preserved order. Exercise
   short bursts and at least 60 seconds at the highest supported sustained load.
4. Repeat with standard and extended identifiers, DLC 0 through 8, remote frames,
   timestamps enabled, and firmware with and without packet padding. Check
   timestamp order across the 32-bit microsecond wrap (about 72 minutes).
5. Send frames while the bus is otherwise idle and during the receive load.
   Verify on the separate adapter that transmission remains responsive. Also
   pause the pipe reader long enough to fill host queues and verify TX/control
   still work; losses after exhausting finite buffers are expected in this case.
6. Unplug the USB adapter while idle and while receiving. Confirm the server
   reports the error and exits without a retry spin. Repeat Ctrl+C and reopen to
   check that cancellation releases the USB handle cleanly.
