//! Battery / charger source via `Windows.System.Power.PowerManager`.
//!
//! PowerManager raises events when the battery status, the power supply or the
//! remaining charge change; we re-read the (cheap, synchronous) properties on
//! each one. A transition of the power supply turns into the charger-connected
//! / charger-disconnected alerts, like UPower's `OnBattery` on Linux.

use async_trait::async_trait;
use tokio::sync::mpsc;
use windows::Foundation::EventHandler;
use windows::System::Power::{BatteryStatus, PowerManager, PowerSupplyStatus};

use mira_core::error::{Error, Result};
use mira_core::event::{AppEvent, EventBus};
use mira_core::model::{Power, PowerState, SystemEvent};
use mira_core::source::{PowerSource, Source};

#[derive(Default)]
pub struct WinPowerSource;

impl WinPowerSource {
    pub fn new() -> Self {
        WinPowerSource
    }
}

impl PowerSource for WinPowerSource {}

#[async_trait]
impl Source for WinPowerSource {
    fn name(&self) -> &'static str {
        "power"
    }

    async fn run(self: Box<Self>, bus: EventBus) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let handler = || {
            let tx = tx.clone();
            EventHandler::new(move |_, _| {
                let _ = tx.send(());
                Ok(())
            })
        };
        // The registrations live as long as the process; we never unregister.
        PowerManager::BatteryStatusChanged(&handler()).map_err(err)?;
        PowerManager::PowerSupplyStatusChanged(&handler()).map_err(err)?;
        PowerManager::RemainingChargePercentChanged(&handler()).map_err(err)?;

        let mut last = read_power();
        bus.publish(AppEvent::Power(last));

        while rx.recv().await.is_some() {
            let now = read_power();
            if now.has_battery && now.on_ac != last.on_ac {
                let percentage = now.percentage;
                bus.publish(AppEvent::System(if now.on_ac {
                    SystemEvent::ChargerConnected { percentage }
                } else {
                    SystemEvent::ChargerDisconnected { percentage }
                }));
            }
            if now != last {
                bus.publish(AppEvent::Power(now));
                last = now;
            }
        }
        Ok(())
    }
}

fn read_power() -> Power {
    let battery = PowerManager::BatteryStatus().unwrap_or(BatteryStatus::NotPresent);
    let supply = PowerManager::PowerSupplyStatus().unwrap_or(PowerSupplyStatus::NotPresent);
    let percentage = PowerManager::RemainingChargePercent().unwrap_or(0).clamp(0, 100) as f64;
    let has_battery = battery != BatteryStatus::NotPresent;
    let state = if !has_battery {
        PowerState::Unknown
    } else if battery == BatteryStatus::Charging {
        PowerState::Charging
    } else if battery == BatteryStatus::Discharging {
        if percentage <= 0.0 {
            PowerState::Empty
        } else {
            PowerState::Discharging
        }
    } else if percentage >= 99.0 {
        // Idle on AC with a full battery.
        PowerState::FullyCharged
    } else {
        PowerState::Unknown
    };
    Power {
        has_battery,
        percentage,
        state,
        on_ac: supply != PowerSupplyStatus::NotPresent,
    }
}

fn err(e: windows::core::Error) -> Error {
    Error::other(format!("power: {e}"))
}
