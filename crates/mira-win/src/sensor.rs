//! The laptop's time-of-flight sensor (ST VL53L1 on Lenovo's sensor hub).
//!
//! Lenovo publishes it to Windows as *custom* sensors, readable by any app
//! through `Windows.Devices.Sensors.Custom`. `windows-sys` has no WinRT, so
//! the handful of calls are made over the raw ABI (slot numbers from the
//! Windows SDK metadata). A worker thread polls the distance and posts:
//!
//! * [`WM_SENSOR_AWAY`] once the user has been gone (> 1 m) for a few seconds;
//! * [`WM_SENSOR_SWIPE`] when a hand passes close (< 15 cm) to the sensor.
//!
//! The hub only refreshes about once a second, so a swipe is caught when a
//! reading lands while the hand is in front of the screen.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

pub const WM_SENSOR_AWAY: u32 = WM_APP + 40;
pub const WM_SENSOR_SWIPE: u32 = WM_APP + 41;

/// Raw ToF (field 162 = distance, mm) and the "HOD" view (field 161).
const TOF_SENSOR: GUID = GUID::from_u128(0x00009c40_766d_4333_8262_27e82dd158b1);
const HOD_SENSOR: GUID = GUID::from_u128(0x00000226_766d_4333_8262_27e82dd158b1);
const TOF_DISTANCE: &str = "{C458F8A7-4AE8-4777-9607-2E9BDD65110A} 162";
const HOD_DISTANCE: &str = "{C458F8A7-4AE8-4777-9607-2E9BDD65110A} 161";

/// Farther than this counts as "nobody at the laptop".
pub const AWAY_MM: u32 = 1000;
/// Closer than this counts as a hand in front of the sensor.
pub const SWIPE_MM: u32 = 150;
/// How long the user must stay away before locking.
const AWAY_FOR: Duration = Duration::from_secs(3);
/// Coming back within this distance re-arms the away lock.
const BACK_MM: u32 = 900;
const SWIPE_COOLDOWN: Duration = Duration::from_millis(2500);

// --- detection (pure) ----------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Away,
    Swipe,
}

/// Turns a stream of distance samples into events.
pub struct Detector {
    pub away_enabled: bool,
    pub swipe_enabled: bool,
    far_since: Option<Instant>,
    away_fired: bool,
    last_swipe: Option<Instant>,
    /// Last reading that was *not* a hand — the swipe must start from there.
    resting_mm: Option<u32>,
}

impl Detector {
    pub fn new() -> Self {
        Detector {
            away_enabled: false,
            swipe_enabled: false,
            far_since: None,
            away_fired: false,
            last_swipe: None,
            resting_mm: None,
        }
    }

    /// Feed one poll: `far_mm` drives "away" (the raw ToF), `near_mm` is the
    /// closest of the sensors (catches a hand as early as possible).
    pub fn feed(
        &mut self,
        far_mm: Option<u32>,
        near_mm: Option<u32>,
        now: Instant,
    ) -> Option<Event> {
        if let Some(near) = near_mm.filter(|d| *d > 0) {
            if near < SWIPE_MM {
                let rested = self.resting_mm.is_some_and(|r| r >= SWIPE_MM + 100);
                let cooled = self
                    .last_swipe
                    .map_or(true, |t| now.duration_since(t) >= SWIPE_COOLDOWN);
                if self.swipe_enabled && rested && cooled {
                    self.last_swipe = Some(now);
                    // The hand would otherwise read as "not away" anyway.
                    return Some(Event::Swipe);
                }
            } else {
                self.resting_mm = Some(near);
            }
        }
        let far = far_mm.filter(|d| *d > 0)?;
        if far > AWAY_MM {
            let since = *self.far_since.get_or_insert(now);
            if self.away_enabled && !self.away_fired && now.duration_since(since) >= AWAY_FOR {
                self.away_fired = true;
                return Some(Event::Away);
            }
        } else {
            self.far_since = None;
            if far < BACK_MM {
                self.away_fired = false;
            }
        }
        None
    }
}

// --- WinRT over the raw ABI -------------------------------------------------------

type Raw = *mut c_void;
type HString = *mut c_void;

// Not in windows-sys; linked the same way it links everything else.
windows_targets::link!("combase.dll" "system" fn RoInitialize(kind: i32) -> HRESULT);
windows_targets::link!("combase.dll" "system" fn RoGetActivationFactory(class: HString, iid: *const GUID, factory: *mut Raw) -> HRESULT);
windows_targets::link!("combase.dll" "system" fn WindowsCreateString(s: *const u16, len: u32, out: *mut HString) -> HRESULT);
windows_targets::link!("combase.dll" "system" fn WindowsDeleteString(s: HString) -> HRESULT);

const IID_ICUSTOMSENSORSTATICS: GUID = GUID::from_u128(0x992052cf_f422_4c7d_836b_e7dc74a7124b);
const IID_IDEVICEINFORMATIONSTATICS: GUID = GUID::from_u128(0xc17f100e_3a46_4a78_8013_769dc9b97390);
const IID_IASYNCINFO: GUID = GUID::from_u128(0x00000036_0000_0000_c000_000000000046);
const IID_IPROPERTYVALUE: GUID = GUID::from_u128(0x4bd682dd_7554_40e9_9a9b_82654ede7e62);

unsafe fn slot<F: Copy>(obj: Raw, n: usize) -> F {
    let vtbl = *(obj as *const *const usize);
    std::mem::transmute_copy(&*vtbl.add(n))
}

unsafe fn release(obj: Raw) {
    if !obj.is_null() {
        slot::<unsafe extern "system" fn(Raw) -> u32>(obj, 2)(obj);
    }
}

unsafe fn query(obj: Raw, iid: &GUID) -> Raw {
    let mut out = null_mut();
    slot::<unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT>(obj, 0)(
        obj, iid, &mut out,
    );
    out
}

/// An owned HSTRING.
struct HStr(HString);

impl HStr {
    fn new(s: &str) -> Self {
        let w: Vec<u16> = s.encode_utf16().collect();
        let mut h = null_mut();
        unsafe { WindowsCreateString(w.as_ptr(), w.len() as u32, &mut h) };
        HStr(h)
    }
}

impl Drop for HStr {
    fn drop(&mut self) {
        unsafe { WindowsDeleteString(self.0) };
    }
}

unsafe fn factory(class: &str, iid: &GUID) -> Option<Raw> {
    let name = HStr::new(class);
    let mut f = null_mut();
    (RoGetActivationFactory(name.0, iid, &mut f) == S_OK && !f.is_null()).then_some(f)
}

/// Wait for an `IAsyncOperation<T>` and return its result (`GetResults`).
unsafe fn wait(op: Raw) -> Option<Raw> {
    if op.is_null() {
        return None;
    }
    let info = query(op, &IID_IASYNCINFO);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = 0i32;
    while !info.is_null() && Instant::now() < deadline {
        slot::<unsafe extern "system" fn(Raw, *mut i32) -> HRESULT>(info, 7)(info, &mut status);
        if status != 0 {
            break; // 1 = completed, 2 = cancelled, 3 = error
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    release(info);
    let mut result = null_mut();
    let ok = status == 1
        && slot::<unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT>(op, 8)(op, &mut result)
            == S_OK;
    release(op);
    (ok && !result.is_null()).then_some(result)
}

/// Open the first custom sensor with interface `id` (an `ICustomSensor`).
unsafe fn open_custom(statics: Raw, devices: Raw, id: GUID) -> Option<Raw> {
    let mut selector = null_mut();
    slot::<unsafe extern "system" fn(Raw, GUID, *mut HString) -> HRESULT>(statics, 6)(
        statics,
        id,
        &mut selector,
    );
    let mut op = null_mut();
    slot::<unsafe extern "system" fn(Raw, HString, *mut Raw) -> HRESULT>(devices, 10)(
        devices, selector, &mut op,
    );
    WindowsDeleteString(selector);
    let list = wait(op)?; // IVectorView<DeviceInformation>
    let mut size = 0u32;
    slot::<unsafe extern "system" fn(Raw, *mut u32) -> HRESULT>(list, 7)(list, &mut size);
    let mut info = null_mut();
    if size > 0 {
        slot::<unsafe extern "system" fn(Raw, u32, *mut Raw) -> HRESULT>(list, 6)(
            list, 0, &mut info,
        );
    }
    release(list);
    if info.is_null() {
        return None;
    }
    let mut device_id = null_mut();
    slot::<unsafe extern "system" fn(Raw, *mut HString) -> HRESULT>(info, 6)(info, &mut device_id);
    release(info);
    let mut op = null_mut();
    slot::<unsafe extern "system" fn(Raw, HString, *mut Raw) -> HRESULT>(statics, 7)(
        statics, device_id, &mut op,
    );
    WindowsDeleteString(device_id);
    // Only read: the report interval belongs to Lenovo's sensing service,
    // and changing it can make the hub stop ranging.
    let sensor = wait(op)?;
    Some(sensor)
}

/// Numeric field `key` of the sensor's current reading.
unsafe fn read_field(sensor: Raw, key: &HStr) -> Option<u32> {
    let mut reading = null_mut();
    slot::<unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT>(sensor, 6)(sensor, &mut reading);
    if reading.is_null() {
        return None;
    }
    let mut props = null_mut();
    slot::<unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT>(reading, 7)(reading, &mut props);
    release(reading);
    if props.is_null() {
        return None;
    }
    let mut value = null_mut();
    slot::<unsafe extern "system" fn(Raw, HString, *mut Raw) -> HRESULT>(props, 6)(
        props, key.0, &mut value,
    );
    release(props);
    if value.is_null() {
        return None;
    }
    let pv = query(value, &IID_IPROPERTYVALUE);
    release(value);
    if pv.is_null() {
        return None;
    }
    let mut kind = 0i32;
    slot::<unsafe extern "system" fn(Raw, *mut i32) -> HRESULT>(pv, 6)(pv, &mut kind);
    let out = match kind {
        // UInt8, Int16, UInt16, Int32, UInt32, Int64, UInt64, Single, Double
        1 => get::<u8>(pv, 8).map(u32::from),
        2 => get::<i16>(pv, 9).map(|v| v.max(0) as u32),
        3 => get::<u16>(pv, 10).map(u32::from),
        4 => get::<i32>(pv, 11).map(|v| v.max(0) as u32),
        5 => get::<u32>(pv, 12),
        6 => get::<i64>(pv, 13).map(|v| v.clamp(0, u32::MAX as i64) as u32),
        7 => get::<u64>(pv, 14).map(|v| v.min(u32::MAX as u64) as u32),
        8 => get::<f32>(pv, 15).map(|v| v.max(0.) as u32),
        9 => get::<f64>(pv, 16).map(|v| v.max(0.) as u32),
        _ => None,
    };
    release(pv);
    out
}

unsafe fn get<T: Default>(pv: Raw, n: usize) -> Option<T> {
    let mut v = T::default();
    (slot::<unsafe extern "system" fn(Raw, *mut T) -> HRESULT>(pv, n)(pv, &mut v) == S_OK)
        .then_some(v)
}

// --- worker -------------------------------------------------------------------------

/// Running sensor watcher; dropping it stops the thread.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    away: Arc<AtomicBool>,
    swipe: Arc<AtomicBool>,
}

impl Watcher {
    pub fn set_modes(&self, away: bool, swipe: bool) {
        self.away.store(away, Ordering::Relaxed);
        self.swipe.store(swipe, Ordering::Relaxed);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start polling the ToF sensor. Returns an error message when the laptop
/// has no such sensor (checked before returning).
pub fn start(hwnd: HWND, away: bool, swipe: bool) -> Result<Watcher, String> {
    let watcher = Watcher {
        stop: Arc::new(AtomicBool::new(false)),
        away: Arc::new(AtomicBool::new(away)),
        swipe: Arc::new(AtomicBool::new(swipe)),
    };
    let (stop, away_flag, swipe_flag) = (
        watcher.stop.clone(),
        watcher.away.clone(),
        watcher.swipe.clone(),
    );
    let hwnd = hwnd as isize;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || unsafe {
        RoInitialize(1); // multi-threaded apartment
        let opened = (|| {
            let statics = factory(
                "Windows.Devices.Sensors.Custom.CustomSensor",
                &IID_ICUSTOMSENSORSTATICS,
            )?;
            let devices = factory(
                "Windows.Devices.Enumeration.DeviceInformation",
                &IID_IDEVICEINFORMATIONSTATICS,
            )?;
            let tof = open_custom(statics, devices, TOF_SENSOR);
            let hod = open_custom(statics, devices, HOD_SENSOR);
            release(statics);
            release(devices);
            (tof.is_some() || hod.is_some()).then_some((tof, hod))
        })();
        let Some((tof, hod)) = opened else {
            let _ = ready_tx.send(Err("No time-of-flight sensor found on this laptop.".into()));
            return;
        };
        let _ = ready_tx.send(Ok(()));
        let (tof_key, hod_key) = (HStr::new(TOF_DISTANCE), HStr::new(HOD_DISTANCE));
        let mut detector = Detector::new();
        while !stop.load(Ordering::Relaxed) {
            detector.away_enabled = away_flag.load(Ordering::Relaxed);
            detector.swipe_enabled = swipe_flag.load(Ordering::Relaxed);
            let t = tof.and_then(|s| read_field(s, &tof_key));
            let h = hod.and_then(|s| read_field(s, &hod_key));
            let near = match (t, h) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            match detector.feed(t.or(h), near, Instant::now()) {
                Some(Event::Away) => {
                    PostMessageW(hwnd as HWND, WM_SENSOR_AWAY, 0, 0);
                }
                Some(Event::Swipe) => {
                    PostMessageW(hwnd as HWND, WM_SENSOR_SWIPE, 0, 0);
                }
                None => {}
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        if let Some(s) = tof {
            release(s);
        }
        if let Some(s) = hod {
            release(s);
        }
    });
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(watcher),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("The sensor did not answer.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn locks_after_three_seconds_away_and_rearms() {
        let mut d = Detector::new();
        d.away_enabled = true;
        let t0 = Instant::now();
        assert_eq!(d.feed(Some(600), Some(600), t0), None);
        assert_eq!(d.feed(Some(1240), Some(1240), at(t0, 1000)), None);
        assert_eq!(d.feed(Some(1250), Some(1250), at(t0, 3000)), None);
        assert_eq!(
            d.feed(Some(1250), Some(1250), at(t0, 4100)),
            Some(Event::Away)
        );
        // Only once while still away.
        assert_eq!(d.feed(Some(1250), Some(1250), at(t0, 9000)), None);
        // Back at the desk, then away again: fires again.
        assert_eq!(d.feed(Some(500), Some(500), at(t0, 10_000)), None);
        assert_eq!(d.feed(Some(1300), Some(1300), at(t0, 11_000)), None);
        assert_eq!(
            d.feed(Some(1300), Some(1300), at(t0, 14_100)),
            Some(Event::Away)
        );
    }

    #[test]
    fn leaning_back_briefly_does_not_lock() {
        let mut d = Detector::new();
        d.away_enabled = true;
        let t0 = Instant::now();
        d.feed(Some(1100), Some(1100), t0);
        d.feed(Some(1100), Some(1100), at(t0, 2000));
        assert_eq!(d.feed(Some(700), Some(700), at(t0, 2500)), None);
        assert_eq!(d.feed(Some(1100), Some(1100), at(t0, 4000)), None);
    }

    #[test]
    fn swipe_needs_a_resting_distance_and_cools_down() {
        let mut d = Detector::new();
        d.swipe_enabled = true;
        let t0 = Instant::now();
        // A hand already there at start isn't a swipe.
        assert_eq!(d.feed(Some(100), Some(100), t0), None);
        assert_eq!(d.feed(Some(600), Some(600), at(t0, 1000)), None);
        assert_eq!(
            d.feed(Some(600), Some(90), at(t0, 2000)),
            Some(Event::Swipe)
        );
        // Hand still there / again right away: no repeat.
        assert_eq!(d.feed(Some(80), Some(80), at(t0, 2100)), None);
        assert_eq!(d.feed(Some(600), Some(600), at(t0, 3000)), None);
        assert_eq!(d.feed(Some(100), Some(100), at(t0, 4000)), None);
        assert_eq!(d.feed(Some(600), Some(600), at(t0, 5000)), None);
        assert_eq!(
            d.feed(Some(100), Some(100), at(t0, 6000)),
            Some(Event::Swipe)
        );
    }

    /// Talks to the real sensor; run on a laptop that has one with
    /// `cargo test -p mira-win -- --ignored live_sensor`.
    #[test]
    #[ignore]
    fn live_sensor_reads_distance() {
        unsafe {
            RoInitialize(1);
            let statics = factory(
                "Windows.Devices.Sensors.Custom.CustomSensor",
                &IID_ICUSTOMSENSORSTATICS,
            )
            .expect("CustomSensor factory");
            let devices = factory(
                "Windows.Devices.Enumeration.DeviceInformation",
                &IID_IDEVICEINFORMATIONSTATICS,
            )
            .expect("DeviceInformation factory");
            let tof = open_custom(statics, devices, TOF_SENSOR).expect("ToF sensor");
            let key = HStr::new(TOF_DISTANCE);
            let mm = read_field(tof, &key).expect("distance field");
            println!("ToF distance: {mm} mm");
            assert!(mm > 0 && mm < 5000);
            release(tof);
            release(statics);
            release(devices);
        }
    }

    #[test]
    fn disabled_modes_stay_quiet() {
        let mut d = Detector::new();
        let t0 = Instant::now();
        d.feed(Some(600), Some(600), t0);
        assert_eq!(d.feed(Some(100), Some(100), at(t0, 1000)), None);
        d.feed(Some(1300), Some(1300), at(t0, 2000));
        assert_eq!(d.feed(Some(1300), Some(1300), at(t0, 6000)), None);
    }
}
