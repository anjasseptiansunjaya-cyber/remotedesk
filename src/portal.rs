//! Wayland screen capture + input injection through xdg-desktop-portal.

use std::{os::fd::OwnedFd, path::Path};

use anyhow::{Context, Result};
use enumflags2::BitFlags;
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};

use crate::keymap;

pub struct Portal {
    rd: RemoteDesktop,
    session: Session<RemoteDesktop>,
    pub node_id: u32,
    pub pw_fd: OwnedFd,
    /// Logical size of the shared stream (coordinate space for absolute pointer).
    pub size: (i32, i32),
}

impl Portal {
    pub async fn start(token_file: &Path) -> Result<Self> {
        let rd = RemoteDesktop::new().await?;
        let sc = Screencast::new().await?;
        let session = rd.create_session(Default::default()).await?;

        let token = std::fs::read_to_string(token_file).ok();
        rd.select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref().map(str::trim)),
        )
        .await?;
        sc.select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false),
        )
        .await?;

        eprintln!("Menunggu persetujuan berbagi layar di komputer ini (dialog portal)...");
        let resp = rd
            .start(&session, None, Default::default())
            .await?
            .response()?;
        if let Some(t) = resp.restore_token() {
            let _ = std::fs::write(token_file, t);
        }
        eprintln!("Perangkat input yang diizinkan: {:?}", resp.devices());
        let stream = resp.streams().first().context("portal tidak memberi stream layar")?;
        let size = stream.size().unwrap_or((1920, 1080));
        let node_id = stream.pipe_wire_node_id();
        let pw_fd = sc.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok(Self { rd, session, node_id, pw_fd, size })
    }

    pub async fn pointer_move(&self, x: f64, y: f64) -> Result<()> {
        let (w, h) = self.size;
        let (x, y) = (x.clamp(0.0, 1.0) * w as f64, y.clamp(0.0, 1.0) * h as f64);
        self.rd
            .notify_pointer_motion_absolute(&self.session, self.node_id, x, y, Default::default())
            .await?;
        Ok(())
    }

    /// `button`: 0 left, 1 middle, 2 right (browser numbering).
    pub async fn pointer_button(&self, button: u8, down: bool) -> Result<()> {
        let code = match button {
            0 => 0x110,
            1 => 0x112,
            2 => 0x111,
            3 => 0x113,
            4 => 0x114,
            _ => return Ok(()),
        };
        self.rd
            .notify_pointer_button(&self.session, code, state(down), Default::default())
            .await?;
        Ok(())
    }

    pub async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        if dy != 0 {
            self.rd
                .notify_pointer_axis_discrete(&self.session, Axis::Vertical, dy, Default::default())
                .await?;
        }
        if dx != 0 {
            self.rd
                .notify_pointer_axis_discrete(&self.session, Axis::Horizontal, dx, Default::default())
                .await?;
        }
        Ok(())
    }

    pub async fn key(&self, code: &str, down: bool) -> Result<()> {
        if let Some(k) = keymap::evdev(code) {
            self.rd
                .notify_keyboard_keycode(&self.session, k, state(down), Default::default())
                .await?;
        }
        Ok(())
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        for c in text.chars() {
            let sym = keymap::keysym(c);
            self.rd
                .notify_keyboard_keysym(&self.session, sym, KeyState::Pressed, Default::default())
                .await?;
            self.rd
                .notify_keyboard_keysym(&self.session, sym, KeyState::Released, Default::default())
                .await?;
        }
        Ok(())
    }
}

fn state(down: bool) -> KeyState {
    if down { KeyState::Pressed } else { KeyState::Released }
}
