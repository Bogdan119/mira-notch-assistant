//! Phone link: a tiny HTTP endpoint on the local network so the Mira phone
//! app can trigger the "shake → sleep → lock" animation.
//!
//! * Off by default; turned on from the robot's right-click menu.
//! * Every request must carry the pairing code (`X-Mira-Code`), which is
//!   random, persisted in `config.toml`, and shown only on this screen (as a
//!   QR code). Wrong codes are rate-limited.
//! * The only thing the phone can do is ask for the animation, which ends in
//!   `LockWorkStation` — locking, never unlocking.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

use crate::wide;

pub const PORT: u16 = 47800;
/// Posted to the notch window when the phone asks for the animation.
pub const WM_PHONE_SHAKE: u32 = WM_APP + 30;

/// A fresh 6-digit pairing code from the system CSPRNG.
pub fn new_code() -> String {
    let mut b = [0u8; 4];
    unsafe {
        BCryptGenRandom(
            null_mut(),
            b.as_mut_ptr(),
            4,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        );
    }
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

/// This machine's address on the LAN (the interface used for outbound
/// traffic). Connecting a UDP socket sends nothing.
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.0.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// What the phone scans: `mira://<ip>:<port>?code=<code>`.
pub fn pairing_uri(code: &str) -> Option<String> {
    Some(format!("mira://{}:{PORT}?code={code}", lan_ip()?))
}

/// Running server; dropping it stops accepting connections.
pub struct Server {
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the accept() call.
        let _ =
            TcpStream::connect_timeout(&([127, 0, 0, 1], PORT).into(), Duration::from_millis(200));
    }
}

/// Start listening on all interfaces. `hwnd` receives [`WM_PHONE_SHAKE`].
pub fn start(code: String, hwnd: HWND) -> std::io::Result<Server> {
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let hwnd = hwnd as isize;
    std::thread::spawn(move || {
        let mut failures: Vec<Instant> = Vec::new();
        for stream in listener.incoming() {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else {
                continue;
            };
            // Ten wrong codes in a minute lock pairing out for that minute.
            failures.retain(|t| t.elapsed() < Duration::from_secs(60));
            let locked_out = failures.len() >= 10;
            match handle(stream, &code, locked_out) {
                Request::Shake => unsafe {
                    PostMessageW(hwnd as HWND, WM_PHONE_SHAKE, 0, 0);
                },
                Request::BadCode => failures.push(Instant::now()),
                Request::Other => {}
            }
        }
    });
    Ok(Server { stop })
}

enum Request {
    Shake,
    BadCode,
    Other,
}

fn handle(stream: TcpStream, code: &str, locked_out: bool) -> Request {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.len() > 512 {
        return Request::Other;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut given = String::new();
    for _ in 0..32 {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("x-mira-code") {
                given = v.trim().to_string();
            }
        }
    }
    let respond = |status: &str, body: &str| {
        let mut s = &stream;
        let _ = write!(
            s,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    };
    if locked_out {
        respond("429 Too Many Requests", r#"{"ok":false,"error":"locked"}"#);
        return Request::Other;
    }
    if !constant_time_eq(given.as_bytes(), code.as_bytes()) {
        respond("401 Unauthorized", r#"{"ok":false,"error":"code"}"#);
        return Request::BadCode;
    }
    match (method, path) {
        ("GET", "/ping") => {
            respond("200 OK", r#"{"ok":true,"name":"Mira"}"#);
            Request::Other
        }
        ("POST", "/shake") => {
            respond("200 OK", r#"{"ok":true}"#);
            Request::Shake
        }
        _ => {
            respond("404 Not Found", r#"{"ok":false}"#);
            Request::Other
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// --- pairing window with the QR code --------------------------------------------

struct QrData {
    modules: Vec<bool>,
    width: usize,
    lines: [String; 2],
}

thread_local! {
    static QR: std::cell::RefCell<Option<QrData>> = const { std::cell::RefCell::new(None) };
}

/// Show (or bring back) a small window with the pairing QR code.
pub unsafe fn show_pairing(owner: HWND, code: &str, ro: bool) {
    let class = wide("MiraPhonePairing");
    let existing = FindWindowW(class.as_ptr(), null());
    if !existing.is_null() {
        DestroyWindow(existing);
    }
    let Some(uri) = pairing_uri(code) else {
        MessageBoxW(
            owner,
            wide(if ro {
                "Nu găsesc rețeaua Wi-Fi a laptopului."
            } else {
                "Couldn't find the laptop's Wi-Fi network."
            })
            .as_ptr(),
            wide("Mira").as_ptr(),
            MB_ICONWARNING,
        );
        return;
    };
    let Ok(qr) = qrcode::QrCode::new(uri.as_bytes()) else {
        return;
    };
    let width = qr.width();
    let modules = qr
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    let address = uri
        .trim_start_matches("mira://")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();
    QR.with(|q| {
        *q.borrow_mut() = Some(QrData {
            modules,
            width,
            lines: [
                if ro {
                    "Scanează din aplicația Mira de pe telefon".into()
                } else {
                    "Scan it from the Mira phone app".into()
                },
                format!("{address}   ·   cod {code}"),
            ],
        })
    });

    let instance = GetModuleHandleW(null());
    let wc = WNDCLASSW {
        lpfnWndProc: Some(pairing_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: GetStockObject(WHITE_BRUSH) as HBRUSH,
        ..std::mem::zeroed()
    };
    RegisterClassW(&wc);
    let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(owner).max(96) as i32;
    let size = 360 * dpi / 96;
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST,
        class.as_ptr(),
        wide(if ro {
            "Mira · Conectează telefonul"
        } else {
            "Mira · Connect your phone"
        })
        .as_ptr(),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        size,
        size + 90 * dpi / 96,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

unsafe extern "system" fn pairing_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            QR.with(|q| {
                let q = q.borrow();
                let Some(q) = q.as_ref() else {
                    return;
                };
                let text_h = 64;
                let avail = (rc.right - rc.left).min(rc.bottom - rc.top - text_h);
                // Quiet zone of 4 modules on each side, as the spec asks.
                let cell = (avail / (q.width as i32 + 8)).max(1);
                let side = cell * q.width as i32;
                let x0 = (rc.right - side) / 2;
                let y0 = (avail - side) / 2 + 4;
                let black = GetStockObject(BLACK_BRUSH) as HBRUSH;
                for (i, dark) in q.modules.iter().enumerate() {
                    if *dark {
                        let (x, y) = ((i % q.width) as i32, (i / q.width) as i32);
                        let r = RECT {
                            left: x0 + x * cell,
                            top: y0 + y * cell,
                            right: x0 + (x + 1) * cell,
                            bottom: y0 + (y + 1) * cell,
                        };
                        FillRect(dc, &r, black);
                    }
                }
                SetBkMode(dc, TRANSPARENT as i32);
                let font = GetStockObject(DEFAULT_GUI_FONT);
                let old = SelectObject(dc, font);
                for (n, line) in q.lines.iter().enumerate() {
                    let mut r = RECT {
                        left: 0,
                        top: avail + 4 + n as i32 * 24,
                        right: rc.right,
                        bottom: avail + 28 + n as i32 * 24,
                    };
                    let w = wide(line);
                    DrawTextW(
                        dc,
                        w.as_ptr(),
                        (w.len() - 1) as i32,
                        &mut r,
                        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                    );
                }
                SelectObject(dc, old);
            });
            EndPaint(hwnd, &ps);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let c = new_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
    }

    #[test]
    fn compares_codes() {
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123457"));
        assert!(!constant_time_eq(b"12345", b"123456"));
    }

    #[test]
    fn serves_ping_and_rejects_bad_codes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut results = Vec::new();
            for _ in 0..3 {
                let (s, _) = listener.accept().unwrap();
                results.push(handle(s, "424242", false));
            }
            results
        });
        let send = |req: &str| {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            std::io::Read::read_to_string(&mut s, &mut out).unwrap();
            out
        };
        assert!(send("GET /ping HTTP/1.1\r\nX-Mira-Code: 424242\r\n\r\n").contains("200 OK"));
        assert!(send("POST /shake HTTP/1.1\r\nX-Mira-Code: 000000\r\n\r\n").contains("401"));
        assert!(send("POST /shake HTTP/1.1\r\nx-mira-code: 424242\r\n\r\n").contains("200 OK"));
        let r = server.join().unwrap();
        assert!(matches!(r[1], Request::BadCode));
        assert!(matches!(r[2], Request::Shake));
    }
}
