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
unsafe fn find_gs_usb_interfaces(
    device: *mut libusb::libusb_device,
) -> io::Result<Vec<InterfaceInfo>> {
    let config = match unsafe { ConfigDescriptor::active(device) } {
        Ok(cfg) => cfg,
        Err(err) if err.kind() == io::ErrorKind::NotFound => unsafe {
            ConfigDescriptor::by_index(device, 0)?
        },
        Err(err) => return Err(err),
    };

    let mut found = Vec::new();
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
                    found.push(info);
                    break; // One entry per interface, rather than per alternate setting.
                }
            }
        }
    }

    Ok(found)
}

/// A connected gs_usb adapter interface, including interfaces that cannot be opened.
#[derive(Clone, Debug)]
pub struct GsUsbAdapter {
    /// USB bus/address/interface identifier, e.g. `003:013/0`.
    pub id: String,
    pub interface: Option<u8>,
    pub product: Option<String>,
    pub serial: Option<String>,
    /// `None` means that opening and claiming the interface succeeded during discovery.
    pub unavailable_reason: Option<String>,
}

impl GsUsbAdapter {
    pub fn is_available(&self) -> bool {
        self.unavailable_reason.is_none()
    }

    pub fn label(&self) -> &str {
        self.product
            .as_deref()
            .or(self.serial.as_deref())
            .unwrap_or("gs_usb adapter")
    }
}

struct DeviceList {
    ptr: *const *mut libusb::libusb_device,
    len: usize,
}

impl DeviceList {
    fn new(context: &LibusbContext) -> io::Result<Self> {
        let mut ptr = ptr::null();
        let len = unsafe { libusb::libusb_get_device_list(context.ptr.0, &mut ptr) };
        if len < 0 {
            return Err(map_libusb_error(len as i32));
        }
        Ok(Self {
            ptr,
            len: len as usize,
        })
    }

    fn candidates(&self) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for i in 0..self.len {
            let device = unsafe { *self.ptr.add(i) };
            let desc = match get_device_descriptor(device) {
                Ok(desc) => desc,
                Err(_) => continue, // An unrelated unreadable device must not stop discovery.
            };
            if desc.idVendor != 0x1d50 || desc.idProduct != 0x606f {
                continue;
            }
            let address = unsafe {
                format!(
                    "{:03}:{:03}",
                    libusb::libusb_get_bus_number(device),
                    libusb::libusb_get_device_address(device)
                )
            };
            match unsafe { find_gs_usb_interfaces(device) } {
                Ok(interfaces) if !interfaces.is_empty() => {
                    for info in interfaces {
                        candidates.push(Candidate {
                            device,
                            id: format!("{address}/{}", info.interface),
                            address: address.clone(),
                            info: Some(info),
                            product_index: desc.iProduct,
                            serial_index: desc.iSerialNumber,
                            descriptor_error: None,
                        });
                    }
                }
                result => {
                    let reason = match result {
                        Err(error) => format!("Cannot read USB interfaces: {error}"),
                        Ok(_) => "No compatible gs_usb bulk interface".into(),
                    };
                    candidates.push(Candidate {
                        device,
                        id: address.clone(),
                        address,
                        info: None,
                        product_index: desc.iProduct,
                        serial_index: desc.iSerialNumber,
                        descriptor_error: Some(reason),
                    });
                }
            }
        }
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        candidates
    }
}

impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe {
            libusb::libusb_free_device_list(self.ptr, 1);
        }
    }
}

struct Candidate {
    device: *mut libusb::libusb_device,
    id: String,
    address: String,
    info: Option<InterfaceInfo>,
    product_index: u8,
    serial_index: u8,
    descriptor_error: Option<String>,
}

struct Opened<T> {
    value: T,
    product: Option<String>,
    serial: Option<String>,
}

fn open_candidate(
    context: &Arc<LibusbContext>,
    candidate: &Candidate,
) -> io::Result<Opened<(LibusbDeviceHandle, InterfaceInfo)>> {
    let info = candidate.info.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            candidate.descriptor_error.clone().unwrap_or_default(),
        )
    })?;
    let ownership = crate::instance_lock::InstanceLock::acquire("gsusb", &candidate.id)?;
    let handle = LibusbDeviceHandle::open(context.clone(), candidate.device, ownership)?;
    let _ = handle.set_auto_detach_kernel_driver(true);
    // Claim only this interface. Discovery sends no gs_usb configuration commands.
    handle.claim_interface(i32::from(info.interface))?;
    Ok(Opened {
        product: read_string_descriptor(&handle, candidate.product_index),
        serial: read_string_descriptor(&handle, candidate.serial_index),
        value: (handle, info),
    })
}

fn unavailable_reason(error: &io::Error) -> String {
    let description = match error.kind() {
        io::ErrorKind::AddrInUse => "In use",
        io::ErrorKind::PermissionDenied => "In use or access denied",
        io::ErrorKind::Unsupported => "Unsupported interface or USB driver",
        io::ErrorKind::NotConnected => "Disconnected during discovery",
        _ => "Cannot open interface",
    };
    format!("{description}: {error}")
}

pub(crate) fn list_adapters(context: &Arc<LibusbContext>) -> io::Result<Vec<GsUsbAdapter>> {
    let devices = DeviceList::new(context)?;
    Ok(devices
        .candidates()
        .iter()
        .map(|candidate| {
            let mut adapter = GsUsbAdapter {
                id: candidate.id.clone(),
                interface: candidate.info.map(|i| i.interface),
                product: None,
                serial: None,
                unavailable_reason: None,
            };
            match open_candidate(context, candidate) {
                Ok(opened) => {
                    adapter.product = opened.product;
                    adapter.serial = opened.serial;
                    // The probe handle drops here, releasing both interface and reservation.
                }
                Err(error) => adapter.unavailable_reason = Some(unavailable_reason(&error)),
            }
            adapter
        })
        .collect())
}

/// Match selectors that do not require opening a USB handle first. `None` means
/// that the selector could be a product string or serial number.
fn static_match(candidate: &Candidate, index: usize, identifier: &str) -> Option<bool> {
    if identifier.eq_ignore_ascii_case("auto") {
        return Some(true);
    }
    if let Ok(selected_index) = identifier.parse::<usize>() {
        return Some(index == selected_index);
    }
    if identifier.eq_ignore_ascii_case(&candidate.id)
        || identifier.eq_ignore_ascii_case(&candidate.address)
    {
        return Some(true);
    }
    // A USB location selector must never open unrelated adapters to identify them.
    if identifier.contains(':') {
        return Some(false);
    }
    None
}

fn identity_matches<T>(candidate: &Candidate, opened: &Opened<T>, identifier: &str) -> bool {
    let (name, interface) = match identifier.rsplit_once('/') {
        Some((name, iface)) => (name, iface.parse::<u8>().ok()),
        None => (identifier, None),
    };
    if let Some(interface) = interface {
        if candidate.info.map(|i| i.interface) != Some(interface) {
            return false;
        }
    }
    [opened.product.as_deref(), opened.serial.as_deref()]
        .into_iter()
        .flatten()
        .any(|s| s.eq_ignore_ascii_case(name))
}

fn choose_candidate<T>(
    candidates: &[Candidate],
    identifier: &str,
    mut open: impl FnMut(&Candidate) -> io::Result<Opened<T>>,
) -> io::Result<(usize, Opened<T>)> {
    let identifier = identifier.trim();
    let automatic = identifier.eq_ignore_ascii_case("auto");
    let mut failures = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let location_match = static_match(candidate, index, identifier);
        if location_match == Some(false) {
            continue;
        }
        match open(candidate) {
            Ok(opened)
                if location_match == Some(true)
                    || identity_matches(candidate, &opened, identifier) =>
            {
                return Ok((index, opened));
            }
            Ok(_) => {}
            Err(error) => {
                let reason = format!("{}: {}", candidate.id, unavailable_reason(&error));
                if location_match == Some(true) && !automatic {
                    return Err(io::Error::new(error.kind(), reason));
                }
                failures.push(reason);
            }
        }
    }
    let details = if failures.is_empty() {
        String::new()
    } else {
        format!("\n{}", failures.join("\n"))
    };
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "No available gs_usb adapter matched '{identifier}'. Use --list-adapters to inspect connected adapters.{details}"
        ),
    ))
}

pub(crate) fn select_device(
    context: &Arc<LibusbContext>,
    identifier: &str,
) -> io::Result<(LibusbDeviceHandle, InterfaceInfo, String)> {
    let devices = DeviceList::new(context)?;
    let candidates = devices.candidates();
    let (index, opened) = choose_candidate(&candidates, identifier, |candidate| {
        open_candidate(context, candidate)
    })?;
    let label = opened
        .product
        .or(opened.serial)
        .unwrap_or_else(|| candidates[index].id.clone());
    let (handle, info) = opened.value;
    log::info!(
        "Selected gs_usb {}: iface={} in_ep=0x{:02x} out_ep=0x{:02x}",
        candidates[index].id,
        info.interface,
        info.in_ep,
        info.out_ep
    );
    Ok((handle, info, label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str) -> Candidate {
        Candidate {
            device: ptr::null_mut(),
            id: id.into(),
            address: id.split('/').next().unwrap().into(),
            info: Some(InterfaceInfo {
                interface: 0,
                alt_setting: 0,
                in_ep: 0x81,
                out_ep: 2,
                int_ep: None,
                out_wmax: 64,
            }),
            product_index: 0,
            serial_index: 0,
            descriptor_error: None,
        }
    }

    fn opened(value: usize) -> Opened<usize> {
        Opened {
            value,
            product: Some("CANable".into()),
            serial: Some(format!("serial-{value}")),
        }
    }

    #[test]
    fn selecting_second_adapter_never_opens_busy_first_adapter() {
        let candidates = [candidate("001:001/0"), candidate("001:002/0")];
        for selector in ["001:002/0", "1"] {
            let (index, result) = choose_candidate(&candidates, selector, |c| {
                assert_eq!(c.id, "001:002/0", "opened an unrelated adapter");
                Ok(opened(2))
            })
            .unwrap();
            assert_eq!(index, 1);
            assert_eq!(result.value, 2);
        }
    }

    #[test]
    fn automatic_and_serial_selection_skip_unavailable_adapters() {
        let candidates = [candidate("001:001/0"), candidate("001:002/0")];
        for selector in ["auto", "serial-2"] {
            let (index, _) = choose_candidate(&candidates, selector, |c| {
                if c.id == "001:001/0" {
                    Err(io::Error::new(io::ErrorKind::AddrInUse, "busy"))
                } else {
                    Ok(opened(2))
                }
            })
            .unwrap();
            assert_eq!(index, 1);
        }
    }

    #[test]
    fn explicitly_selected_busy_adapter_is_not_replaced() {
        let candidates = [candidate("001:001/0"), candidate("001:002/0")];
        let error = choose_candidate(&candidates, "001:001/0", |c| -> io::Result<Opened<usize>> {
            assert_eq!(c.id, "001:001/0");
            Err(io::Error::new(io::ErrorKind::AddrInUse, "busy"))
        })
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains("001:001/0"));
    }
}
