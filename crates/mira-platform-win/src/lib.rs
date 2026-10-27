//! Windows data sources for Mira.
//!
//! * [`gsmtc`] — media playback via the Global System Media Transport Controls
//!   (the same API the Windows volume flyout uses), plus playback control.
//! * [`power`] — battery / charger via `Windows.System.Power.PowerManager`.
//!
//! Both are event-driven: they register WinRT event handlers and only read
//! state when Windows says it changed. The crate is empty on other OSes so the
//! workspace builds everywhere.

#[cfg(windows)]
pub mod gsmtc;
#[cfg(windows)]
pub mod power;

#[cfg(windows)]
pub use gsmtc::{new_gsmtc, GsmtcControl, GsmtcSource};
#[cfg(windows)]
pub use power::WinPowerSource;
