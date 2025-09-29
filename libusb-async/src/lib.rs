use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusb::{self, DeviceDescriptor, Direction, GlobalContext, Recipient, RequestType, TransferType};
use tokio::task;

#[derive(Debug)]
pub enum UsbAsyncError {
    Usb(rusb::Error),
    Join(tokio::task::JoinError),
}

impl From<rusb::Error> for UsbAsyncError {
    fn from(err: rusb::Error) -> Self {
        UsbAsyncError::Usb(err)
    }
}

impl From<tokio::task::JoinError> for UsbAsyncError {
    fn from(err: tokio::task::JoinError) -> Self {
        UsbAsyncError::Join(err)
    }
}

impl fmt::Display for UsbAsyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsbAsyncError::Usb(err) => write!(f, "USB operation failed: {err}"),
            UsbAsyncError::Join(err) => write!(f, "USB task join error: {err}"),
        }
    }
}

impl std::error::Error for UsbAsyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UsbAsyncError::Usb(err) => Some(err),
            UsbAsyncError::Join(err) => Some(err),
        }
    }
}

impl From<UsbAsyncError> for io::Error {
    fn from(err: UsbAsyncError) -> Self {
        match err {
            UsbAsyncError::Usb(rusb::Error::Timeout) => {
                io::Error::new(io::ErrorKind::WouldBlock, rusb::Error::Timeout)
            }
            UsbAsyncError::Usb(rusb::Error::Pipe) => {
                io::Error::new(io::ErrorKind::BrokenPipe, rusb::Error::Pipe)
            }
            UsbAsyncError::Usb(rusb::Error::NoDevice) => {
                io::Error::new(io::ErrorKind::NotConnected, rusb::Error::NoDevice)
            }
            UsbAsyncError::Usb(other) => io::Error::new(io::ErrorKind::Other, other),
            UsbAsyncError::Join(e) => {
                io::Error::new(io::ErrorKind::Other, format!("USB task join error: {e}"))
            }
        }
    }
}

pub use DeviceDescriptor;
pub use Direction;
pub use Recipient;
pub use RequestType;
pub use TransferType;

pub fn request_type(direction: Direction, request_type: RequestType, recipient: Recipient) -> u8 {
    rusb::request_type(direction, request_type, recipient)
}

#[derive(Clone)]
pub struct Device {
    inner: rusb::Device<GlobalContext>,
}

impl Device {
    fn new(inner: rusb::Device<GlobalContext>) -> Self {
        Self { inner }
    }

    pub fn bus_number(&self) -> u8 {
        self.inner.bus_number()
    }

    pub fn address(&self) -> u8 {
        self.inner.address()
    }

    pub async fn device_descriptor(&self) -> Result<DeviceDescriptor, UsbAsyncError> {
        let device = self.inner.clone();
        task::spawn_blocking(move || Ok(device.device_descriptor()?)).await??
    }

    pub async fn active_config_descriptor(&self) -> Result<rusb::ConfigDescriptor, UsbAsyncError> {
        let device = self.inner.clone();
        task::spawn_blocking(move || Ok(device.active_config_descriptor()?)).await??
    }

    pub async fn config_descriptor(&self, index: u8) -> Result<rusb::ConfigDescriptor, UsbAsyncError> {
        let device = self.inner.clone();
        task::spawn_blocking(move || Ok(device.config_descriptor(index)?)).await??
    }

    pub async fn open(&self) -> Result<DeviceHandle, UsbAsyncError> {
        let device = self.inner.clone();
        let handle = task::spawn_blocking(move || Ok(device.open()?)).await??;
        Ok(DeviceHandle::new(handle))
    }
}

pub async fn devices() -> Result<Vec<Device>, UsbAsyncError> {
    task::spawn_blocking(|| {
        let list = rusb::devices()?;
        let mut out = Vec::with_capacity(list.len());
        for device in list.iter() {
            out.push(Device::new(device));
        }
        Ok(out)
    })
    .await??
}

#[derive(Clone)]
pub struct DeviceHandle {
    inner: Arc<Mutex<rusb::DeviceHandle<GlobalContext>>>,
}

impl DeviceHandle {
    fn new(handle: rusb::DeviceHandle<GlobalContext>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(handle)),
        }
    }

    async fn with_handle<T, F>(&self, f: F) -> Result<T, UsbAsyncError>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusb::DeviceHandle<GlobalContext>) -> Result<T, rusb::Error> + Send + 'static,
    {
        let handle = self.inner.clone();
        task::spawn_blocking(move || {
            let mut guard = handle
                .lock()
                .map_err(|_| rusb::Error::Other)?;
            f(&mut guard)
        })
        .await?
        .map_err(UsbAsyncError::from)
    }

    pub async fn set_auto_detach_kernel_driver(&self, enable: bool) -> Result<(), UsbAsyncError> {
        self.with_handle(move |handle| {
            handle.set_auto_detach_kernel_driver(enable).map_err(|e| match e {
                rusb::Error::NotSupported => rusb::Error::Other,
                other => other,
            })?;
            Ok(())
        })
        .await
    }

    pub async fn claim_interface(&self, interface: u8) -> Result<(), UsbAsyncError> {
        self.with_handle(move |handle| {
            handle.claim_interface(interface)?;
            Ok(())
        })
        .await
    }

    pub async fn release_interface(&self, interface: u8) -> Result<(), UsbAsyncError> {
        self.with_handle(move |handle| {
            handle.release_interface(interface)?;
            Ok(())
        })
        .await
    }

    pub async fn write_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbAsyncError> {
        let data = data.to_vec();
        self.with_handle(move |handle| handle.write_control(request_type, request, value, index, &data, timeout))
            .await
    }

    pub async fn read_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbAsyncError> {
        let ptr = data.as_mut_ptr();
        let len = data.len();
        self.with_handle(move |handle| {
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
            handle.read_control(request_type, request, value, index, slice, timeout)
        })
        .await
    }

    pub async fn write_bulk(
        &self,
        endpoint: u8,
        data: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbAsyncError> {
        let ptr = data.as_ptr();
        let len = data.len();
        self.with_handle(move |handle| {
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            handle.write_bulk(endpoint, slice, timeout)
        })
        .await
    }

    pub async fn read_bulk(
        &self,
        endpoint: u8,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbAsyncError> {
        let ptr = buffer.as_mut_ptr();
        let len = buffer.len();
        self.with_handle(move |handle| {
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
            handle.read_bulk(endpoint, slice, timeout)
        })
        .await
    }

    pub async fn clear_halt(&self, endpoint: u8) -> Result<(), UsbAsyncError> {
        self.with_handle(move |handle| {
            handle.clear_halt(endpoint)?;
            Ok(())
        })
        .await
    }

    pub async fn read_serial_number_string_ascii(
        &self,
        descriptor: &DeviceDescriptor,
    ) -> Result<String, UsbAsyncError> {
        let desc = descriptor.clone();
        self.with_handle(move |handle| Ok(handle.read_serial_number_string_ascii(&desc)?))
            .await
    }

    pub async fn read_product_string_ascii(
        &self,
        descriptor: &DeviceDescriptor,
    ) -> Result<String, UsbAsyncError> {
        let desc = descriptor.clone();
        self.with_handle(move |handle| Ok(handle.read_product_string_ascii(&desc)?))
            .await
    }
}

pub fn map_usb_err(err: UsbAsyncError) -> io::Error {
    err.into()
}

pub type ConfigDescriptor = rusb::ConfigDescriptor;

pub type InterfaceDescriptor<'a> = rusb::InterfaceDescriptor<'a>;

pub type EndpointDescriptor<'a> = rusb::EndpointDescriptor<'a>;
