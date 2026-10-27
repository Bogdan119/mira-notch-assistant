//! The detached robot's personality: how it follows, and what it gets up to
//! when roaming freely (wandering, looking around, dozing off, coughing,
//! checking on the laptop). Pure state machine; the host applies the moves.

use std::time::{Duration, Instant};

use windows_sys::Win32::{
    Foundation::FILETIME,
    System::{
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
        SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        Threading::GetSystemTimes,
    },
};

/// What the robot does once it is out of the notch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PetKind {
    /// Trails the pointer at a polite distance.
    Follow,
    /// Stays where it is put (it can still be dragged around).
    Pinned,
    /// Wanders the screen and does its own thing.
    Roam,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Eyes {
    Open,
    Closed,
    Squint,
}

/// Tiny xorshift generator: plenty for picking moods.
pub struct Rng(u64);

impl Rng {
    pub fn seeded() -> Self {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9);
        Rng(t | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (self.next() % 10_000) as f32 / 10_000. * (hi - lo)
    }

    pub fn ms(&mut self, lo: u64, hi: u64) -> Duration {
        Duration::from_millis(lo + self.next() % (hi - lo).max(1))
    }
}

/// One roaming activity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    /// Travel to a point on screen (device px).
    Wander {
        x: f32,
        y: f32,
        since: Instant,
    },
    /// Glance around (at the desktop icons, a window…), changing spot often.
    Look {
        until: Instant,
        next: Instant,
    },
    Sleep {
        until: Instant,
    },
    /// Show a thought bubble with the laptop's vitals.
    Stats {
        until: Instant,
    },
    Cough {
        start: Instant,
    },
    Idle {
        until: Instant,
    },
}

impl Act {
    pub fn eyes(&self, now: Instant) -> Eyes {
        match self {
            Act::Sleep { .. } => Eyes::Closed,
            Act::Cough { start } if now.duration_since(*start) < Duration::from_millis(1100) => {
                Eyes::Squint
            }
            _ => Eyes::Open,
        }
    }
}

/// Pick the next roaming activity. `area` is the work area (l, t, r, b) in
/// device px, `margin` keeps the robot fully on screen.
pub fn next_act(
    rng: &mut Rng,
    now: Instant,
    area: (f32, f32, f32, f32),
    margin: (f32, f32),
) -> Act {
    let roll = rng.next() % 100;
    match roll {
        0..=37 => Act::Wander {
            x: rng.range(area.0 + margin.0, area.2 - margin.0),
            y: rng.range(area.1 + margin.1 + 40., area.3 - margin.1),
            since: now,
        },
        38..=53 => Act::Look {
            until: now + rng.ms(2500, 4500),
            next: now,
        },
        54..=63 => Act::Sleep {
            until: now + rng.ms(4000, 9000),
        },
        64..=77 => Act::Stats {
            until: now + rng.ms(3500, 5000),
        },
        78..=86 => Act::Cough { start: now },
        _ => Act::Idle {
            until: now + rng.ms(1000, 2600),
        },
    }
}

/// Whole-machine CPU load from successive `GetSystemTimes` samples.
pub struct CpuMeter {
    last: Option<(u64, u64)>,
    pub percent: u32,
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

impl CpuMeter {
    pub fn new() -> Self {
        CpuMeter {
            last: None,
            percent: 0,
        }
    }

    /// Take a sample; the reading covers the time since the previous one.
    pub unsafe fn sample(&mut self) {
        let (mut idle, mut kernel, mut user) = std::mem::zeroed::<(FILETIME, FILETIME, FILETIME)>();
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return;
        }
        // Kernel time includes idle time.
        let (idle, busy_total) = (ft(idle), ft(kernel) + ft(user));
        if let Some((li, lt)) = self.last {
            let total = busy_total.saturating_sub(lt);
            let idle_d = idle.saturating_sub(li);
            if let Some(idle_pct) = (idle_d * 100).checked_div(total) {
                self.percent = (100 - idle_pct.min(100)) as u32;
            }
        }
        self.last = Some((idle, busy_total));
    }
}

/// "CPU 23% · RAM 61% · Bat 80%+" for the thought bubble.
pub unsafe fn vitals(cpu: u32) -> String {
    let mut mem: MEMORYSTATUSEX = std::mem::zeroed();
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ram = if GlobalMemoryStatusEx(&mut mem) != 0 {
        mem.dwMemoryLoad
    } else {
        0
    };
    let mut text = format!("CPU {cpu}% · RAM {ram}%");
    let mut power: SYSTEM_POWER_STATUS = std::mem::zeroed();
    if GetSystemPowerStatus(&mut power) != 0 && power.BatteryLifePercent <= 100 {
        let plug = if power.ACLineStatus == 1 { "+" } else { "" };
        text.push_str(&format!(" · Bat {}%{plug}", power.BatteryLifePercent));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_stays_in_range() {
        let mut r = Rng::seeded();
        for _ in 0..1000 {
            let v = r.range(10., 20.);
            assert!((10. ..20.).contains(&v));
        }
    }

    #[test]
    fn wander_targets_stay_on_screen() {
        let mut r = Rng::seeded();
        let now = Instant::now();
        for _ in 0..500 {
            if let Act::Wander { x, y, .. } =
                next_act(&mut r, now, (0., 0., 1920., 1040.), (80., 30.))
            {
                assert!((80. ..=1840.).contains(&x));
                assert!((70. ..=1010.).contains(&y));
            }
        }
    }

    #[test]
    fn cpu_meter_reads_a_percentage() {
        let mut m = CpuMeter::new();
        unsafe {
            m.sample();
            std::thread::sleep(Duration::from_millis(50));
            m.sample();
        }
        assert!(m.percent <= 100);
        assert!(unsafe { vitals(m.percent) }.starts_with("CPU "));
    }
}
