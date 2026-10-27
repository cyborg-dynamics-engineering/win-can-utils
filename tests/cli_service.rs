//! Smoke test the installed command workflow as separate Windows processes.
use crosscan::{
    protocol::{Command as Request, Reply, Subscription},
    service::ServiceClient,
};
use std::os::windows::process::CommandExt;
use std::{
    io,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

fn command(binary: &str) -> Command {
    let mut command = Command::new(binary);
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct ConfigFile(PathBuf);
impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn saved_config_cli_send_dump_and_state_commands() -> io::Result<()> {
    let name = format!("cli_{}", std::process::id());
    let path = ConfigFile(
        std::env::current_dir()?
            .join("target")
            .join(format!("{name}.json")),
    );
    let create = command(env!("CARGO_BIN_EXE_canctl"))
        .args([
            &name,
            "create",
            "--driver",
            "virtual",
            "--device",
            "virtual",
            "--bitrate",
            "500000",
            "--config",
        ])
        .arg(&path.0)
        .output()?;
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let _server = Process(
        command(env!("CARGO_BIN_EXE_canserver"))
            .arg("--config")
            .arg(&path.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let observer = ServiceClient::connect(
        &name,
        Subscription {
            receive: false,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        matches!(observer.request(Request::Status).await?, Reply::Status { status } if !status.desired_up)
    );
    for op in ["up", "show", "clients"] {
        let result = command(env!("CARGO_BIN_EXE_canctl"))
            .args([&name, op])
            .output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let mut dump = Process(
        command(env!("CARGO_BIN_EXE_candump"))
            .args(["-n", "1", &name])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Reply::Clients { clients } = observer.request(Request::Clients).await.unwrap() {
                if clients.len() >= 2 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let send = command(env!("CARGO_BIN_EXE_cansend"))
        .args([&name, "123#010203", "--receive-own-messages"])
        .output()?;
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );
    assert!(String::from_utf8_lossy(&send.stdout).contains("local_tx"));
    tokio::time::timeout(Duration::from_secs(3), async {
        while dump.0.try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(dump.0.stdout.as_mut().unwrap(), &mut text)?;
    assert!(text.contains("01 02 03") && text.contains("TX"), "{text}");
    let down = command(env!("CARGO_BIN_EXE_canctl"))
        .args([&name, "down"])
        .output()?;
    assert!(down.status.success());
    let set = command(env!("CARGO_BIN_EXE_canctl"))
        .args([&name, "set", "--bitrate", "250000", "--timestamps", "false"])
        .output()?;
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let config = win_can_utils::config::InterfaceConfig::load(&path.0)?;
    assert_eq!(config.bitrate, 250_000);
    assert!(!config.up && !config.timestamps);
    Ok(())
}
