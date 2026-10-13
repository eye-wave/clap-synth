//! The only platform-specific code: a child window inside the host's window, plus a Vulkan
//! surface for it. Every backend exposes the same API:
//!   API / EXTS      CLAP gui api name, Vulkan instance extensions to enable
//!   Native::new     create the child window under the host's parent window; dropping a .wav
//!                   file on it loads it into the given Wave (Windows: WM_DROPFILES, X11: XDND)
//!                   clicking the box opens a file picker (Windows: GetOpenFileNameW, X11: zenity/kdialog)
//!   show / hide     map / unmap it
//!   surface         create the Vulkan surface
//! Dropping a `Native` destroys the window (on the thread that created it: the host's main thread).
use crate::Wave;
use ash::{khr, vk, Entry, Instance};
use clack_extensions::gui::{GuiApiType, Window};
use clack_plugin::prelude::PluginError;
use std::ffi::CStr;
use std::sync::Arc;

pub const W: u32 = 400;
pub const H: u32 = 300;

/// Is the window pixel (x, y) inside the box drawn by gui.rs? (Click -> file picker.)
pub fn in_box(x: i32, y: i32) -> bool {
    use crate::gui::{BX, BY};
    (2.0 * x as f32 / W as f32 - 1.0).abs() < BX && (2.0 * y as f32 / H as f32 - 1.0).abs() < BY
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
compile_error!("GUI backends exist for Windows and X11 only");
#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::*;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;
