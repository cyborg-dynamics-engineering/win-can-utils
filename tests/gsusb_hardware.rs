//! Opt-in lifecycle validation on two connected, isolated CAN adapters.
use crosscan::can::CanFrame;
use std::{io, time::Duration};
use win_can_utils::{CanDriver, GsUsbDriver};

async fn reopen(identifier: &str) -> io::Result<GsUsbDriver> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match GsUsbDriver::open(identifier).await {
            Ok(driver) => return Ok(driver),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

#[tokio::test]
#[ignore = "Requires GSUSB_TEST_A and GSUSB_TEST_B on an isolated, wired CAN bus"]
async fn queued_transmit_close_drop_and_reopen() -> io::Result<()> {
    let first =
        std::env::var("GSUSB_TEST_A").expect("Set GSUSB_TEST_A to the first adapter selector");
    let second =
        std::env::var("GSUSB_TEST_B").expect("Set GSUSB_TEST_B to the second adapter selector");
    assert_ne!(first, second);
    for cycle in 0..6u32 {
        let mut a = reopen(&first).await?;
        let mut b = reopen(&second).await?;
        for driver in [&mut a, &mut b] {
            driver.set_bitrate(1_000_000).await?;
            driver.enable_timestamp().await?;
            driver.open_channel().await?;
        }
        // Queue a burst, then close or drop without awaiting individual writes.
        // Loss is intentional here; the separate load test checks delivery.
        for sequence in 0..10u32 {
            a.send_frame(&CanFrame::new(0x5a1, &(cycle * 10 + sequence).to_le_bytes()).unwrap())
                .await?;
        }
        if cycle % 2 == 0 {
            a.close_channel().await?;
            a.open_channel().await?;
            a.send_frame(&CanFrame::new(0x5a1, &[0]).unwrap()).await?;
        }
        drop(a);
        drop(b);
    }
    // A successful reopen confirms that previous worker threads and their
    // transfer-owned handles released both interface reservations.
    let mut a = reopen(&first).await?;
    let mut b = reopen(&second).await?;
    a.close_channel().await?;
    b.close_channel().await?;
    Ok(())
}
