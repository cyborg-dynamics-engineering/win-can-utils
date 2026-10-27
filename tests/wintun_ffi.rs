//! Validate the real runtime FFI and ownership against an ordinary user-mode DLL.
use std::{io, os::windows::process::CommandExt, path::PathBuf, process::Command, time::Duration};
use win_can_utils::network_adapter::{PacketDevice, wintun::Api};

#[test]
fn native_api_keeps_adapter_and_library_alive_until_the_session_ends() -> io::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = root
        .join("target")
        .join(format!("wintun-ffi-{}", std::process::id()));
    let result = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-File"])
        .arg(root.join("scripts/build-wintun-test-double.ps1"))
        .arg("-OutputDirectory")
        .arg(&output)
        .creation_flags(0x08000000)
        .output()?;
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let path = output.join("wincan-test-double.dll");
    let api = Api::load(&path)?;
    let inspection = unsafe { libloading::Library::new(&path) }.unwrap();
    let live_adapters = unsafe {
        inspection.get::<unsafe extern "system" fn() -> u32>(b"WinCanTestLiveAdapters\0")
    }
    .unwrap();
    let live_sessions = unsafe {
        inspection.get::<unsafe extern "system" fn() -> u32>(b"WinCanTestLiveSessions\0")
    }
    .unwrap();
    let creates =
        unsafe { inspection.get::<unsafe extern "system" fn() -> u32>(b"WinCanTestCreates\0") }
            .unwrap();
    let probe = api.probe()?;
    assert_eq!(probe.running_driver_version.as_deref(), Some("1.2"));
    assert!(api.adapter("unknown", false).is_err());
    assert!(api.adapter("bad\0name", false).is_err());
    if !probe.elevated {
        assert_eq!(
            api.adapter("new", true).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            unsafe { creates() },
            0,
            "Elevation gate called adapter creation"
        );
    }
    let adapter = api.adapter("WinCAN mock", false)?;
    assert!(adapter.start(123).is_err());
    let session = adapter.start(0x20000)?;
    assert_eq!(unsafe { live_adapters() }, 1);
    assert_eq!(unsafe { live_sessions() }, 1);
    drop(api);
    drop(adapter);
    assert!(session.receive(Duration::from_millis(1))?.is_none());
    let data = vec![0x45; 100];
    session.send(&data)?;
    assert_eq!(session.receive(Duration::from_millis(10))?, Some(data));
    assert!(session.send(&[]).is_err());
    assert!(session.send(&vec![0; 65_536]).is_err());
    assert_eq!(
        unsafe { live_adapters() },
        1,
        "Adapter dropped while a session still uses it"
    );
    drop(session);
    assert_eq!(unsafe { live_sessions() }, 0);
    assert_eq!(unsafe { live_adapters() }, 0);
    Ok(())
}
