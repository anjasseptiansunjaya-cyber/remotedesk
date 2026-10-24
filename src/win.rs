//! Windows host: input injection through SendInput. Screen capture is ffmpeg (see capture.rs).

use anyhow::{Result, bail};
use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
    },
    WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SetProcessDPIAware,
    },
};

use crate::{capture::Grab, keymap};

const WHEEL_DELTA: i32 = 120;

pub struct Host {
    pub size: (i32, i32),
    /// gdigrab captures the whole virtual desktop, ddagrab only the primary monitor.
    virtual_desk: bool,
}

impl Host {
    pub fn start(grab: Grab) -> Result<Self> {
        let virtual_desk = grab == Grab::Gdi;
        // ddagrab works in physical pixels, so we must be DPI aware to match it.
        // gdigrab (run by a DPI-unaware ffmpeg.exe) sees scaled coordinates, so stay unaware.
        if !virtual_desk {
            unsafe { SetProcessDPIAware() };
        }
        let (w, h) = unsafe {
            if virtual_desk {
                (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
            } else {
                (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
            }
        };
        if w <= 0 || h <= 0 {
            bail!("tidak bisa membaca ukuran layar");
        }
        Ok(Self { size: (w, h), virtual_desk })
    }

    pub async fn pointer_move(&self, x: f64, y: f64) -> Result<()> {
        let abs = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as i32;
        let mut flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
        if self.virtual_desk {
            flags |= MOUSEEVENTF_VIRTUALDESK;
        }
        send(&[mouse(abs(x), abs(y), 0, flags)])
    }

    /// `button`: 0 left, 1 middle, 2 right, 3/4 back/forward (browser numbering).
    pub async fn pointer_button(&self, button: u8, down: bool) -> Result<()> {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (1, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (3, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1), // XBUTTON1
            (4, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2), // XBUTTON2
            _ => return Ok(()),
        };
        send(&[mouse(0, 0, data, flags)])
    }

    /// Positive `dy` scrolls down (browser convention), Windows wants the opposite sign.
    pub async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        let mut v = Vec::new();
        if dy != 0 {
            v.push(mouse(0, 0, (-dy * WHEEL_DELTA) as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            v.push(mouse(0, 0, (dx * WHEEL_DELTA) as u32, MOUSEEVENTF_HWHEEL));
        }
        send(&v)
    }

    pub async fn key(&self, code: &str, down: bool) -> Result<()> {
        let Some((scan, extended)) = scancode(code) else { return Ok(()) };
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[key_input(0, scan, flags)])
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        let mut v = Vec::new();
        for unit in text.encode_utf16() {
            v.push(key_input(0, unit, KEYEVENTF_UNICODE));
            v.push(key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        send(&v)
    }
}

/// evdev keycodes 1..=88 are identical to PS/2 set-1 scancodes; the rest are the E0-prefixed keys.
fn scancode(code: &str) -> Option<(u16, bool)> {
    let k = keymap::evdev(code)?;
    Some(match k {
        1..=88 => (k as u16, false),
        96 => (0x1c, true),  // numpad enter
        97 => (0x1d, true),  // right ctrl
        98 => (0x35, true),  // numpad /
        99 => (0x37, true),  // print screen
        100 => (0x38, true), // right alt
        102 => (0x47, true), // home
        103 => (0x48, true), // up
        104 => (0x49, true), // page up
        105 => (0x4b, true), // left
        106 => (0x4d, true), // right
        107 => (0x4f, true), // end
        108 => (0x50, true), // down
        109 => (0x51, true), // page down
        110 => (0x52, true), // insert
        111 => (0x53, true), // delete
        125 => (0x5b, true), // left win
        126 => (0x5c, true), // right win
        127 => (0x5d, true), // menu
        _ => return None,
    })
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        bail!("SendInput hanya menyuntik {n} dari {} event (jendela dengan hak lebih tinggi/UAC?)", inputs.len());
    }
    Ok(())
}
