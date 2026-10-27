# Local Windows interface ownership changes

Based on the published `libusb1-sys` 0.7.0 crate, bundling libusb 1.0.27.
Original bindings, licenses and source headers are retained.

In `libusb/libusb/os/windows_winusb.c`, WinUSB-like device handles are opened
when claiming an interface, rather than opening every interface in `libusb_open`.
Associated interfaces still open their required first interface. Composite close
only frees handles belonging to the backend being closed, avoiding duplicate
close/free calls on devices mixing WinUSB and libusbK.

This addresses the independent-interface ownership limitation described in
[libusb issue 1177](https://github.com/libusb/libusb/issues/1177). The patch is
local; it is not an upstream libusb release. Cargo forces the vendored build so
an installed system libusb cannot silently bypass these changes.
