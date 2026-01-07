use std::io;
use std::ptr;
use std::sync::Arc;

use libusb1_sys as libusb;
use libusb1_sys::constants::{
    LIBUSB_ENDPOINT_IN, LIBUSB_TRANSFER_TYPE_BULK, LIBUSB_TRANSFER_TYPE_INTERRUPT,
};

use super::context::{
    LibusbContext, LibusbDeviceHandle, get_device_descriptor, map_libusb_error,
    read_string_descriptor,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct InterfaceInfo {
    pub(crate) interface: u8,
    pub(crate) alt_setting: u8,
    pub(crate) in_ep: u8,
    pub(crate) out_ep: u8,
    pub(crate) int_ep: Option<u8>,
    pub(crate) out_wmax: u16,
}

struct ConfigDescriptor(*const libusb::libusb_config_descriptor);

impl ConfigDescriptor {
    unsafe fn active(device: *mut libusb::libusb_device) -> io::Result<Self> {
        let mut ptr = ptr::null();
        let rc = unsafe { libusb::libusb_get_active_config_descriptor(device, &mut ptr) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self(ptr))
    }

    unsafe fn by_index(device: *mut libusb::libusb_device, index: u8) -> io::Result<Self> {
        let mut ptr = ptr::null();
        let rc = unsafe { libusb::libusb_get_config_descriptor(device, index as u8, &mut ptr) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self(ptr))
    }
}

impl Drop for ConfigDescriptor {
    fn drop(&mut self) {
        unsafe { libusb::libusb_free_config_descriptor(self.0) };
    }
}

/// Find the vendor specific gs_usb interface along with the endpoints we need.
unsafe fn find_gs_usb_interface(
    device: *mut libusb::libusb_device,
) -> io::Result<Option<InterfaceInfo>> {
    let config = match unsafe { ConfigDescriptor::active(device) } {
        Ok(cfg) => cfg,
        Err(err) if err.kind() == io::ErrorKind::NotFound => unsafe {
            ConfigDescriptor::by_index(device, 0)?
        },
        Err(err) => return Err(err),
    };

    let config_ptr = config.0;
    let interface_count = unsafe { (*config_ptr).bNumInterfaces };
    for interface_index in 0..interface_count {
        let interface = unsafe { &*(*config_ptr).interface.add(interface_index as usize) };
        for alt_index in 0..interface.num_altsetting as usize {
            unsafe {
                let descriptor = &*interface.altsetting.add(alt_index);
                if descriptor.bInterfaceClass != 0xff {
                    continue;
                }

                let mut info = InterfaceInfo {
                    interface: descriptor.bInterfaceNumber,
                    alt_setting: descriptor.bAlternateSetting, // ← capture it
                    in_ep: 0,
                    out_ep: 0,
                    int_ep: None,
                    out_wmax: 64,
                };

                for ep_index in 0..descriptor.bNumEndpoints as usize {
                    let endpoint = &*descriptor.endpoint.add(ep_index);
                    let xfer_type = endpoint.bmAttributes & 0x3;

                    if xfer_type == LIBUSB_TRANSFER_TYPE_BULK {
                        if endpoint.bEndpointAddress & LIBUSB_ENDPOINT_IN != 0 {
                            info.in_ep = endpoint.bEndpointAddress;
                        } else {
                            info.out_ep = endpoint.bEndpointAddress;
                            info.out_wmax = endpoint.wMaxPacketSize;
                        }
                    } else if xfer_type == LIBUSB_TRANSFER_TYPE_INTERRUPT {
                        if endpoint.bEndpointAddress & LIBUSB_ENDPOINT_IN != 0 {
                            info.int_ep = Some(endpoint.bEndpointAddress);
                        }
                    }
                }

                if info.in_ep != 0 && info.out_ep != 0 {
                    return Ok(Some(info)); // ← pick THIS altsetting
                }
            }
        }
    }

    Ok(None)
}

fn device_matches_identifier(
    identifier: &str,
    index: usize,
    device: *mut libusb::libusb_device,
    desc: &libusb::libusb_device_descriptor,
    handle: &LibusbDeviceHandle,
) -> bool {
    let ident = identifier.trim();
    if ident.eq_ignore_ascii_case("auto") {
        return true;
    }

    if let Ok(idx) = ident.parse::<usize>() {
        if idx == index {
            return true;
        }
    }

    if let Some(serial) = read_string_descriptor(handle, desc.iSerialNumber) {
        if serial.eq_ignore_ascii_case(ident) {
            return true;
        }
    }

    if let Some(product) = read_string_descriptor(handle, desc.iProduct) {
        if product.eq_ignore_ascii_case(ident) {
            return true;
        }
    }

    let bus = unsafe { libusb::libusb_get_bus_number(device) };
    let address = unsafe { libusb::libusb_get_device_address(device) };
    let bus_addr = format!("{:03}:{:03}", bus, address);
    bus_addr.eq_ignore_ascii_case(ident)
}

fn read_product_label(
    handle: &LibusbDeviceHandle,
    desc: &libusb::libusb_device_descriptor,
) -> Option<String> {
    read_string_descriptor(handle, desc.iProduct)
        .or_else(|| read_string_descriptor(handle, desc.iSerialNumber))
        .or_else(|| Some(format!("{:04x}:{:04x}", desc.idVendor, desc.idProduct)))
}

pub(crate) fn select_device(
    context: &Arc<LibusbContext>,
    identifier: &str,
) -> io::Result<(LibusbDeviceHandle, InterfaceInfo, String)> {
    let mut list = ptr::null();
    let count = unsafe { libusb::libusb_get_device_list(context.ptr.0, &mut list) };
    if count < 0 {
        return Err(map_libusb_error(count as i32));
    }

    let mut result: Option<(LibusbDeviceHandle, InterfaceInfo, String)> = None;
    let mut index = 0usize;
    let mut error: Option<io::Error> = None;

    for i in 0..count {
        let device = unsafe { *list.add(i as usize) };
        let desc = match get_device_descriptor(device) {
            Ok(d) => d,
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        if !(desc.idProduct == 0x606F && desc.idVendor == 0x1D50) {
            continue;
        }

        let info = match unsafe { find_gs_usb_interface(device) } {
            Ok(Some(info)) => info,
            Ok(None) => continue,
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        let handle = match LibusbDeviceHandle::open(context.clone(), device) {
            Ok(h) => h,
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        let matches = device_matches_identifier(identifier, index, device, &desc, &handle);

        if matches {
            let label = read_product_label(&handle, &desc)
                .unwrap_or_else(|| format!("{:04x}:{:04x}", desc.idVendor, desc.idProduct));
            result = Some((handle, info, label));
            break;
        }

        index += 1;
    }

    unsafe {
        libusb::libusb_free_device_list(list, 1);
    }

    if let Some((handle, info, label)) = result {
        log::info!(
            "Selected gs_usb iface={} in_ep=0x{:02x} out_ep=0x{:02x} int_ep={:?} out_wmax={}",
            info.interface,
            info.in_ep,
            info.out_ep,
            info.int_ep,
            info.out_wmax
        );
        return Ok((handle, info, label));
    }

    if let Some(err) = error {
        return Err(err);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("No gs_usb device matched identifier '{identifier}'"),
    ))
}
