use std::io;
use std::ptr::null_mut;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_F5, VK_F9, VK_F10,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY};

use crate::replay::Action;

pub struct Hotkeys(Vec<i32>);

impl Hotkeys {
    pub fn prores() -> io::Result<Self> {
        let mut keys = Self(Vec::new());
        for (id, key, modifiers) in [
            (1, VK_F5, MOD_NOREPEAT),
            (2, VK_F10, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT),
        ] {
            // SAFETY: registration belongs to this thread and is released by Drop.
            if unsafe { RegisterHotKey(null_mut(), id, modifiers, u32::from(key)) } == 0 {
                return Err(io::Error::other(format!(
                    "could not register replay hotkey {id}: {}. Close another recorder instance or app using F5.",
                    io::Error::last_os_error()
                )));
            }
            keys.0.push(id);
        }
        Ok(keys)
    }
    pub fn register() -> Self {
        let mut keys = Self(Vec::new());
        for (id, key, label) in [(1, VK_F9, "Ctrl+Alt+F9"), (2, VK_F10, "Ctrl+Alt+F10")] {
            // SAFETY: a null window associates the registration with the calling thread.
            if unsafe {
                RegisterHotKey(
                    null_mut(),
                    id,
                    MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                    u32::from(key),
                )
            } != 0
            {
                keys.0.push(id);
            } else {
                eprintln!(
                    "{label} unavailable ({}). Use the terminal controls.",
                    io::Error::last_os_error()
                );
            }
        }
        keys
    }

    pub fn poll(&self) -> Option<Action> {
        let mut msg = MSG::default();
        // SAFETY: msg is writable and we read only the current thread's hotkey messages.
        if unsafe { PeekMessageW(&mut msg, null_mut(), WM_HOTKEY, WM_HOTKEY, PM_REMOVE) } == 0 {
            return None;
        }
        match msg.wParam {
            1 => Some(Action::Save),
            2 => Some(Action::Quit),
            _ => None,
        }
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        for &id in &self.0 {
            // SAFETY: only successfully registered IDs are stored, and drop runs on the owning thread.
            unsafe { UnregisterHotKey(null_mut(), id) };
        }
    }
}
