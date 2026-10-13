//! The only platform-specific code: a child window inside the host's window, plus a Vulkan
//! surface for it. Every backend exposes the same API:
//!   API / EXTS      CLAP gui api name, Vulkan instance extensions to enable
//!   Native::new     create the child window under the host's parent window
//!   show / hide     map / unmap it
//!   surface         create the Vulkan surface
//! Dropping a `Native` destroys the window (on the thread that created it: the host's main thread).
use ash::{khr, vk, Entry, Instance};
use clack_extensions::gui::{GuiApiType, Window};
use clack_plugin::prelude::PluginError;
use std::ffi::CStr;

pub const W: u32 = 400;
pub const H: u32 = 300;

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
compile_error!("GUI backends exist for Windows and X11 only");

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    use std::ptr::null;
    use x11::xlib;

    pub const API: GuiApiType<'static> = GuiApiType::X11;
    pub const EXTS: [&CStr; 2] = [khr::surface::NAME, khr::xlib_surface::NAME];

    // Display* stored as usize so `Native` is Send + Sync. Xlib is not thread-safe without
    // XInitThreads, so the display is never used from two threads at once: the main thread only
    // touches it in new/show-before-spawn/hide-after-join/drop, and the render thread (surface
    // creation + Vulkan WSI) only runs between show and hide.
    pub struct Native {
        dpy: usize,
        win: xlib::Window,
    }

    impl Native {
        pub fn new(parent: &Window) -> Result<Self, PluginError> {
            let parent = parent
                .as_x11_handle()
                .ok_or(PluginError::Message("not an X11 parent"))?;
            unsafe {
                // Own connection, so we never share the host's Display.
                let dpy = xlib::XOpenDisplay(null());
                if dpy.is_null() {
                    return Err(PluginError::Message("XOpenDisplay failed"));
                }
                let win = xlib::XCreateSimpleWindow(dpy, parent, 0, 0, W, H, 0, 0, 0);
                xlib::XFlush(dpy);
                Ok(Self {
                    dpy: dpy as usize,
                    win,
                })
            }
        }

        pub fn show(&self) -> Result<(), PluginError> {
            unsafe {
                xlib::XMapWindow(self.dpy as _, self.win);
                xlib::XFlush(self.dpy as _);
            }
            Ok(())
        }

        pub fn hide(&self) -> Result<(), PluginError> {
            unsafe {
                xlib::XUnmapWindow(self.dpy as _, self.win);
                xlib::XFlush(self.dpy as _);
            }
            Ok(())
        }

        pub unsafe fn surface(&self, entry: &Entry, inst: &Instance) -> vk::SurfaceKHR {
            khr::xlib_surface::Instance::new(entry, inst)
                .create_xlib_surface(
                    &vk::XlibSurfaceCreateInfoKHR::default()
                        .dpy(self.dpy as _)
                        .window(self.win),
                    None,
                )
                .unwrap()
        }
    }

    impl Drop for Native {
        fn drop(&mut self) {
            unsafe {
                xlib::XDestroyWindow(self.dpy as _, self.win);
                xlib::XCloseDisplay(self.dpy as _);
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    pub const API: GuiApiType<'static> = GuiApiType::WIN32;
    pub const EXTS: [&CStr; 2] = [khr::surface::NAME, khr::win32_surface::NAME];

    // Raw handles stored as usize so `Native` is Send + Sync (the render thread only needs the HWND).
    pub struct Native {
        hwnd: usize,
        hinst: usize,
    }

    impl Native {
        pub fn new(parent: &Window) -> Result<Self, PluginError> {
            let parent = parent
                .as_win32_hwnd()
                .ok_or(PluginError::Message("not a Win32 parent"))?;
            unsafe {
                // Our own DLL's handle (not the host exe's), found from the address of this function.
                let mut hinst: HMODULE = null_mut();
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    Self::new as usize as *const u16,
                    &mut hinst,
                );

                let class: Vec<u16> = "ClapSynthGui\0".encode_utf16().collect();
                let mut wc: WNDCLASSEXW = std::mem::zeroed();
                wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
                wc.lpfnWndProc = Some(DefWindowProcW);
                wc.hInstance = hinst;
                wc.lpszClassName = class.as_ptr();
                // ponytail: fails harmlessly if another plugin instance already registered it; never
                // unregistered. Unregister on DLL unload if that ever matters.
                RegisterClassExW(&wc);

                let hwnd = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    null(),
                    WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                    0,
                    0,
                    W as i32,
                    H as i32,
                    parent as _,
                    null_mut(),
                    hinst,
                    null(),
                );
                if hwnd.is_null() {
                    return Err(PluginError::Message("CreateWindowExW failed"));
                }
                Ok(Self {
                    hwnd: hwnd as usize,
                    hinst: hinst as usize,
                })
            }
        }

        pub fn show(&self) -> Result<(), PluginError> {
            unsafe { ShowWindow(self.hwnd as _, SW_SHOW) };
            Ok(())
        }

        pub fn hide(&self) -> Result<(), PluginError> {
            unsafe { ShowWindow(self.hwnd as _, SW_HIDE) };
            Ok(())
        }

        pub unsafe fn surface(&self, entry: &Entry, inst: &Instance) -> vk::SurfaceKHR {
            khr::win32_surface::Instance::new(entry, inst)
                .create_win32_surface(
                    &vk::Win32SurfaceCreateInfoKHR::default()
                        .hinstance(self.hinst as _)
                        .hwnd(self.hwnd as _),
                    None,
                )
                .unwrap()
        }
    }

    impl Drop for Native {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.hwnd as _) };
        }
    }
}

pub use imp::*;
