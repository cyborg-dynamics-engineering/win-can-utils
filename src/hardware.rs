use crate::{
    CanDriver, GsUsbDriver, PcanDriver, SlcanDriver, config::InterfaceConfig,
    drivers::can_driver::DriverHealth, service::DriverFactory,
};
use async_trait::async_trait;
use crosscan::{
    can::CanFrame,
    protocol::{BusState, Capabilities},
};
use std::{io, sync::Arc};
use tokio::sync::Notify;

pub struct HardwareFactory;

async fn configure(
    mut driver: Box<dyn CanDriver>,
    config: &InterfaceConfig,
) -> io::Result<Box<dyn CanDriver>> {
    driver.set_bitrate(config.bitrate).await?;
    if config.timestamps {
        driver.enable_timestamp().await?;
    }
    if let Err(error) = driver.open_channel().await {
        let _ = driver.close_channel().await;
        return Err(error);
    }
    Ok(driver)
}

#[async_trait]
impl DriverFactory for HardwareFactory {
    async fn open(&self, config: &InterfaceConfig) -> io::Result<Box<dyn CanDriver>> {
        match config.driver.as_str() {
            "virtual" => {
                configure(
                    Box::new(VirtualDriver {
                        bitrate: None,
                        ready: Arc::new(Notify::new()),
                    }),
                    config,
                )
                .await
            }
            "gsusb" => {
                let mut driver = GsUsbDriver::open(&config.device).await?;
                driver.close_channel().await?;
                configure(Box::new(driver), config).await
            }
            "slcan" => {
                let mut driver = SlcanDriver::open(&config.device).await?;
                driver.close_channel().await?;
                configure(Box::new(driver), config).await
            }
            "pcan" => {
                if config.device.eq_ignore_ascii_case("auto") {
                    let mut last = io::Error::new(io::ErrorKind::NotFound, "No PCAN adapter found");
                    for channel in [
                        "USBBUS1", "USBBUS2", "USBBUS3", "USBBUS4", "PCIBUS1", "PCIBUS2",
                        "LANBUS1", "LANBUS2",
                    ] {
                        match PcanDriver::open(channel).await {
                            Ok(driver) => match configure(Box::new(driver), config).await {
                                Ok(driver) => return Ok(driver),
                                Err(error) => last = error,
                            },
                            Err(error) => last = error,
                        }
                    }
                    Err(last)
                } else {
                    configure(Box::new(PcanDriver::open(&config.device).await?), config).await
                }
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unsupported driver",
            )),
        }
    }
}

struct VirtualDriver {
    bitrate: Option<u32>,
    ready: Arc<Notify>,
}

#[async_trait]
impl CanDriver for VirtualDriver {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardware_can_fd: Some(false),
            hardware_timestamps: Some(false),
            bus_state: true,
            software_tx_echo: true,
            can_fd: false,
        }
    }
    async fn health(&mut self) -> io::Result<DriverHealth> {
        Ok(DriverHealth {
            bus_state: BusState::ErrorActive,
            ..Default::default()
        })
    }
    fn receive_notifier(&self) -> Option<Arc<Notify>> {
        Some(self.ready.clone())
    }
    async fn enable_timestamp(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn set_bitrate(&mut self, bitrate: u32) -> io::Result<()> {
        self.bitrate = Some(bitrate);
        Ok(())
    }
    async fn get_bitrate(&self) -> Option<u32> {
        self.bitrate
    }
    async fn open_channel(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn close_channel(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn send_frame(&mut self, _: &CanFrame) -> io::Result<()> {
        Ok(())
    }
    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        Ok(vec![])
    }
}
