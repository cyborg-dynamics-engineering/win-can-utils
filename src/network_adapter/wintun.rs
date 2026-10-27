//! Runtime binding to the official signed Wintun DLL; no driver source is bundled.
use super::{NetworkConfig, PacketDevice};
use libloading::Library;
use serde::Serialize;
use std::{
    ffi::c_void,
    io,
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex},
    time::Duration,
};
use windows_sys::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_BUFFER_OVERFLOW, ERROR_NO_MORE_ITEMS, HANDLE, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        NetworkManagement::{IpHelper::*, Ndis::NET_LUID_LH},
        Networking::WinSock::{
            AF_INET, IpDadStatePreferred, IpPrefixOriginManual, IpSuffixOriginManual,
        },
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken, WaitForSingleObject},
    },
    core::GUID,
};

type CreateAdapter = unsafe extern "system" fn(*const u16, *const u16, *const GUID) -> *mut c_void;
type OpenAdapter = unsafe extern "system" fn(*const u16) -> *mut c_void;
type CloseAdapter = unsafe extern "system" fn(*mut c_void);
type GetAdapterLuid = unsafe extern "system" fn(*mut c_void, *mut NET_LUID_LH);
type StartSession = unsafe extern "system" fn(*mut c_void, u32) -> *mut c_void;
type EndSession = unsafe extern "system" fn(*mut c_void);
type GetReadWaitEvent = unsafe extern "system" fn(*mut c_void) -> HANDLE;
type ReceivePacket = unsafe extern "system" fn(*mut c_void, *mut u32) -> *mut u8;
type ReleasePacket = unsafe extern "system" fn(*mut c_void, *const u8);
type AllocateSendPacket = unsafe extern "system" fn(*mut c_void, u32) -> *mut u8;
type DriverVersion = unsafe extern "system" fn() -> u32;

pub struct Api {
    _library: Library,
    path: PathBuf,
    create: CreateAdapter,
    open: OpenAdapter,
    close: CloseAdapter,
    luid: GetAdapterLuid,
    start: StartSession,
    end: EndSession,
    event: GetReadWaitEvent,
    receive: ReceivePacket,
    release: ReleasePacket,
    allocate: AllocateSendPacket,
    send: ReleasePacket,
    version: DriverVersion,
}

#[derive(Debug, Serialize)]
pub struct Probe {
    pub dll: PathBuf,
    pub api_exports_valid: bool,
    pub elevated: bool,
    pub running_driver_version: Option<String>,
    pub driver_query_error: Option<String>,
}

fn context(message: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(error.kind(), format!("{message}: {error}"))
}

pub fn elevated() -> io::Result<bool> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(context("Open process token"));
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut length = 0;
        let success = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        );
        let error = if success == 0 {
            Some(context("Query elevation"))
        } else {
            None
        };
        CloseHandle(token);
        if let Some(error) = error {
            return Err(error);
        }
        Ok(elevation.TokenIsElevated != 0)
    }
}

pub fn require_elevated() -> io::Result<()> {
    if !elevated()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Creating/configuring a Windows adapter requires an elevated administrator process; no elevation was requested",
        ));
    }
    Ok(())
}

/// Refuse an address plan overlapping any existing unicast interface subnet.
/// This is a read-only preflight, before creating our adapter or adding routes.
pub fn ensure_subnet_unused(network: &NetworkConfig) -> io::Result<()> {
    network.validate()?;
    unsafe {
        let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = ptr::null_mut();
        let code = GetUnicastIpAddressTable(AF_INET, &mut table);
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        if table.is_null() {
            return Err(io::Error::other("IP helper returned a null address table"));
        }
        let rows = std::slice::from_raw_parts(
            ptr::addr_of!((*table).Table).cast::<MIB_UNICASTIPADDRESS_ROW>(),
            (*table).NumEntries as usize,
        );
        let conflict = rows.iter().find_map(|row| {
            if row.Address.si_family != AF_INET || row.OnLinkPrefixLength > 32 {
                return None;
            }
            let address =
                std::net::Ipv4Addr::from(row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
            let common = network.prefix_length.min(row.OnLinkPrefixLength);
            let mask = if common == 0 {
                0
            } else {
                u32::MAX << (32 - common)
            };
            ((u32::from(address) & mask) == (u32::from(network.local) & mask)).then_some(address)
        });
        FreeMibTable(table.cast());
        if let Some(address) = conflict {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "The requested subnet overlaps an existing interface address ({address}); choose another --local/--peer subnet"
                ),
            ));
        }
    }
    Ok(())
}

fn wide_name(name: &str) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    if wide.is_empty() || wide.len() >= 128 || wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid adapter name",
        ));
    }
    Ok(wide.into_iter().chain(Some(0)).collect())
}

impl Api {
    pub fn load(path: &Path) -> io::Result<Arc<Self>> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provide an absolute path to the official signed wintun.dll",
            ));
        }
        let path = path.canonicalize()?;
        // Resolve dependencies only beside the explicitly chosen DLL or in System32.
        let library: Library =
            unsafe { libloading::os::windows::Library::load_with_flags(&path, 0x100 | 0x800) }
                .map_err(io::Error::other)?
                .into();
        unsafe {
            Ok(Arc::new(Self {
                create: *library
                    .get(b"WintunCreateAdapter\0")
                    .map_err(io::Error::other)?,
                open: *library
                    .get(b"WintunOpenAdapter\0")
                    .map_err(io::Error::other)?,
                close: *library
                    .get(b"WintunCloseAdapter\0")
                    .map_err(io::Error::other)?,
                luid: *library
                    .get(b"WintunGetAdapterLUID\0")
                    .map_err(io::Error::other)?,
                start: *library
                    .get(b"WintunStartSession\0")
                    .map_err(io::Error::other)?,
                end: *library
                    .get(b"WintunEndSession\0")
                    .map_err(io::Error::other)?,
                event: *library
                    .get(b"WintunGetReadWaitEvent\0")
                    .map_err(io::Error::other)?,
                receive: *library
                    .get(b"WintunReceivePacket\0")
                    .map_err(io::Error::other)?,
                release: *library
                    .get(b"WintunReleaseReceivePacket\0")
                    .map_err(io::Error::other)?,
                allocate: *library
                    .get(b"WintunAllocateSendPacket\0")
                    .map_err(io::Error::other)?,
                send: *library
                    .get(b"WintunSendPacket\0")
                    .map_err(io::Error::other)?,
                version: *library
                    .get(b"WintunGetRunningDriverVersion\0")
                    .map_err(io::Error::other)?,
                _library: library,
                path,
            }))
        }
    }

    /// Read-only: loads user-mode exports and queries the running driver version.
    pub fn probe(&self) -> io::Result<Probe> {
        let version = unsafe { (self.version)() };
        let error = if version == 0 {
            Some(context("Query running Wintun driver").to_string())
        } else {
            None
        };
        Ok(Probe {
            dll: self.path.clone(),
            api_exports_valid: true,
            elevated: elevated()?,
            running_driver_version: (version != 0)
                .then(|| format!("{}.{}", version >> 16, version & 0xffff)),
            driver_query_error: error,
        })
    }

    pub fn adapter(self: &Arc<Self>, name: &str, create: bool) -> io::Result<Arc<Adapter>> {
        let name = wide_name(name)?;
        if create {
            require_elevated()?;
        }
        let handle = unsafe {
            if create {
                let existing = (self.open)(name.as_ptr());
                if !existing.is_null() {
                    (self.close)(existing);
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Adapter already exists; choose a new dedicated name",
                    ));
                }
                (self.create)(name.as_ptr(), wide_name("WinCAN")?.as_ptr(), ptr::null())
            } else {
                (self.open)(name.as_ptr())
            }
        };
        if handle.is_null() {
            return Err(context("Open/create Wintun adapter"));
        }
        Ok(Arc::new(Adapter {
            api: self.clone(),
            handle,
            created: create,
        }))
    }
}

pub struct Adapter {
    api: Arc<Api>,
    handle: *mut c_void,
    created: bool,
}
// The handle stays alive through Arc ownership; close occurs after the last session.
unsafe impl Send for Adapter {}
unsafe impl Sync for Adapter {}
impl Drop for Adapter {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}

impl Adapter {
    pub fn interface_index(&self) -> io::Result<u32> {
        let luid = self.luid();
        let mut index = 0;
        let code = unsafe { ConvertInterfaceLuidToIndex(&luid, &mut index) };
        if code == 0 {
            Ok(index)
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    fn luid(&self) -> NET_LUID_LH {
        let mut luid = NET_LUID_LH { Value: 0 };
        unsafe {
            (self.api.luid)(self.handle, &mut luid);
        }
        luid
    }

    /// Only adapters created by this process may be configured by this API.
    pub fn configure_ipv4(&self, network: &NetworkConfig) -> io::Result<()> {
        network.validate()?;
        require_elevated()?;
        if !self.created {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IP configuration is restricted to a newly created dedicated adapter",
            ));
        }
        unsafe {
            let mut row: MIB_UNICASTIPADDRESS_ROW = std::mem::zeroed();
            InitializeUnicastIpAddressEntry(&mut row);
            row.InterfaceLuid = self.luid();
            row.Address.Ipv4.sin_family = AF_INET;
            row.Address.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(network.local.octets());
            row.OnLinkPrefixLength = network.prefix_length;
            row.PrefixOrigin = IpPrefixOriginManual;
            row.SuffixOrigin = IpSuffixOriginManual;
            row.DadState = IpDadStatePreferred;
            let code = CreateUnicastIpAddressEntry(&row);
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            let mut interface: MIB_IPINTERFACE_ROW = std::mem::zeroed();
            InitializeIpInterfaceEntry(&mut interface);
            interface.Family = AF_INET;
            interface.InterfaceLuid = self.luid();
            let code = GetIpInterfaceEntry(&mut interface);
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            interface.NlMtu = 65_535;
            interface.SitePrefixLength = 0;
            interface.ForwardingEnabled = 0;
            interface.AdvertiseDefaultRoute = 0;
            let code = SetIpInterfaceEntry(&mut interface);
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        Ok(())
    }

    pub fn start(self: &Arc<Self>, capacity: u32) -> io::Result<Arc<Session>> {
        if !(0x20000..=0x4000000).contains(&capacity) || !capacity.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Wintun ring capacity must be a power of two from 128 KiB through 64 MiB",
            ));
        }
        let handle = unsafe { (self.api.start)(self.handle, capacity) };
        if handle.is_null() {
            return Err(context("Start Wintun packet session"));
        }
        let mut session = Session {
            adapter: self.clone(),
            handle,
            event: ptr::null_mut(),
            send_lock: Mutex::new(()),
        };
        session.event = unsafe { (self.api.event)(handle) };
        if session.event.is_null() {
            return Err(context("Get Wintun receive event"));
        }
        Ok(Arc::new(session))
    }
}

pub struct Session {
    adapter: Arc<Adapter>,
    handle: *mut c_void,
    event: HANDLE,
    send_lock: Mutex<()>,
}
// Wintun's packet functions are thread safe. Send allocation/order is serialized.
// A live Arc protects the session, adapter, event, and function pointers from Drop.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.adapter.api.end)(self.handle);
        }
    }
}

impl Session {
    fn try_receive(&self) -> io::Result<Option<Vec<u8>>> {
        let mut size = 0;
        let packet = unsafe { (self.adapter.api.receive)(self.handle, &mut size) };
        if packet.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_ITEMS as i32) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        if size == 0 || size > 65_535 {
            unsafe {
                (self.adapter.api.release)(self.handle, packet);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Wintun returned an invalid packet length",
            ));
        }
        let data = unsafe { std::slice::from_raw_parts(packet, size as usize) }.to_vec();
        unsafe {
            (self.adapter.api.release)(self.handle, packet);
        }
        Ok(Some(data))
    }
}

impl PacketDevice for Session {
    fn receive(&self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        if let Some(packet) = self.try_receive()? {
            return Ok(Some(packet));
        }
        match unsafe {
            WaitForSingleObject(
                self.event,
                timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32,
            )
        } {
            WAIT_OBJECT_0 => self.try_receive(),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(context("Wait for Wintun receive")),
        }
    }
    fn send(&self, packet: &[u8]) -> io::Result<()> {
        if packet.is_empty() || packet.len() > 65_535 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid layer-3 packet size",
            ));
        }
        let _order = self
            .send_lock
            .lock()
            .map_err(|_| io::Error::other("Wintun send lock poisoned"))?;
        let target = unsafe { (self.adapter.api.allocate)(self.handle, packet.len() as u32) };
        if target.is_null() {
            let error = io::Error::last_os_error();
            return Err(
                if error.raw_os_error() == Some(ERROR_BUFFER_OVERFLOW as i32) {
                    io::Error::new(io::ErrorKind::WouldBlock, "Wintun send ring full")
                } else {
                    error
                },
            );
        }
        unsafe {
            ptr::copy_nonoverlapping(packet.as_ptr(), target, packet.len());
            (self.adapter.api.send)(self.handle, target);
        }
        Ok(())
    }
}
