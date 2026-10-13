//! The only platform-specific code: a child window inside the host's window, plus a Vulkan
//! surface for it. Every backend exposes the same API:
//!   API / EXTS      CLAP gui api name, Vulkan instance extensions to enable
//!   Native::new     create the child window under the host's parent window; dropping a .wav
//!                   file on it loads it into the given Wave (Windows: WM_DROPFILES, X11: XDND)
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

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
compile_error!("GUI backends exist for Windows and X11 only");

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    use std::ffi::OsString;
    use std::os::raw::c_long;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use std::thread::JoinHandle;
    use x11::xlib;

    pub const API: GuiApiType<'static> = GuiApiType::X11;
    pub const EXTS: [&CStr; 2] = [khr::surface::NAME, khr::xlib_surface::NAME];

    // Display* stored as usize so `Native` is Send + Sync. Xlib is not thread-safe without
    // XInitThreads, so each connection has exactly one user at a time:
    //  * `dpy`: main thread (new/show/hide/drop) and, between show and hide, the render thread
    //    (surface creation + Vulkan WSI).
    //  * a second connection that *owns the window* and is used only by the XDND thread. XDND
    //    ClientMessages are delivered to the window's creator, and nobody else reads that
    //    connection's events (the host only reads its own), so it needs its own thread.
    pub struct Native {
        dpy: usize,
        win: xlib::Window,
        dnd: Option<JoinHandle<()>>,
    }

    impl Native {
        pub fn new(parent: &Window, wave: Arc<Wave>) -> Result<Self, PluginError> {
            let parent = parent
                .as_x11_handle()
                .ok_or(PluginError::Message("not an X11 parent"))?;
            unsafe {
                // Own connections, so we never share the host's Display.
                let dpy = xlib::XOpenDisplay(null());
                let dnd = xlib::XOpenDisplay(null());
                if dpy.is_null() || dnd.is_null() {
                    return Err(PluginError::Message("XOpenDisplay failed"));
                }
                let win = xlib::XCreateSimpleWindow(dnd, parent, 0, 0, W, H, 0, 0, 0);
                xlib::XSelectInput(dnd, win, xlib::StructureNotifyMask); // for DestroyNotify
                let aware = xlib::XInternAtom(dnd, c"XdndAware".as_ptr(), 0);
                let version: c_long = 5;
                xlib::XChangeProperty(
                    dnd,
                    win,
                    aware,
                    xlib::XA_ATOM,
                    32,
                    xlib::PropModeReplace,
                    &version as *const c_long as *const u8,
                    1,
                );
                xlib::XFlush(dnd);
                let d = dnd as usize;
                Ok(Self {
                    dpy: dpy as usize,
                    win,
                    dnd: Some(std::thread::spawn(move || xdnd(d as _, win, &wave))),
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
                xlib::XFlush(self.dpy as _);
                // the XDND thread sees the DestroyNotify, exits and closes its own connection
                if let Some(t) = self.dnd.take() {
                    let _ = t.join();
                }
                xlib::XCloseDisplay(self.dpy as _);
            }
        }
    }

    /// "file:///a%20b.wav\r\n..." -> the first path. Percent-decoded, host part dropped.
    fn uri_path(s: &str) -> Option<PathBuf> {
        let u = s.lines().next()?.trim().strip_prefix("file://")?;
        let b = &u.as_bytes()[u.find('/')?..];
        let (mut out, mut i) = (Vec::new(), 0);
        while i < b.len() {
            let hex = (b[i] == b'%')
                .then(|| b.get(i + 1..i + 3))
                .flatten()
                .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
            out.push(hex.unwrap_or(b[i]));
            i += if hex.is_some() { 3 } else { 1 };
        }
        Some(OsString::from_vec(out).into())
    }

    /// Minimal XDND (v5) target: accept any drag over the window, and on drop ask the source for
    /// `text/uri-list` and load the first file from it. Owns `dpy` and closes it on exit.
    // ponytail: no type check on XdndEnter (a non-file drag is "accepted", then the conversion
    // fails and nothing happens), no XdndPosition rectangle, no INCR transfers.
    unsafe fn xdnd(dpy: *mut xlib::Display, win: xlib::Window, wave: &Wave) {
        let atom = |n: &CStr| xlib::XInternAtom(dpy, n.as_ptr(), 0);
        let position = atom(c"XdndPosition");
        let drop = atom(c"XdndDrop");
        let status = atom(c"XdndStatus");
        let finished = atom(c"XdndFinished");
        let selection = atom(c"XdndSelection");
        let copy = atom(c"XdndActionCopy");
        let uri_list = atom(c"text/uri-list");
        let prop = atom(c"XdndData");

        // Reply to the drag source: data.l = [our window, d0..d3]
        let send = |to: xlib::Window, ty: xlib::Atom, d: [c_long; 4]| {
            let mut m: xlib::XClientMessageEvent = std::mem::zeroed();
            m.type_ = xlib::ClientMessage;
            m.display = dpy;
            m.window = to;
            m.message_type = ty;
            m.format = 32;
            m.data.set_long(0, win as c_long);
            for (i, v) in d.into_iter().enumerate() {
                m.data.set_long(i + 1, v);
            }
            let mut e: xlib::XEvent = std::mem::zeroed();
            e.client_message = m;
            xlib::XSendEvent(dpy, to, 0, 0, &mut e);
            xlib::XFlush(dpy);
        };

        let mut src = 0;
        let mut e: xlib::XEvent = std::mem::zeroed();
        loop {
            xlib::XNextEvent(dpy, &mut e);
            match e.get_type() {
                xlib::DestroyNotify => break,
                xlib::ClientMessage => {
                    let m = e.client_message;
                    if m.message_type == position {
                        src = m.data.get_long(0) as xlib::Window;
                        send(src, status, [1, 0, 0, copy as c_long]); // accept, no "stop sending" rect
                    } else if m.message_type == drop {
                        src = m.data.get_long(0) as xlib::Window;
                        let time = m.data.get_long(2) as xlib::Time;
                        xlib::XConvertSelection(dpy, selection, uri_list, prop, win, time);
                    }
                }
                xlib::SelectionNotify => {
                    let ok = e.selection.property != 0;
                    if ok {
                        let (mut ty, mut fmt, mut n, mut left) = (0, 0, 0, 0);
                        let mut p: *mut u8 = null_mut();
                        xlib::XGetWindowProperty(
                            dpy,
                            win,
                            prop,
                            0,
                            1 << 14, // in 32-bit units: 64 KB of URIs is plenty
                            1,       // delete it
                            xlib::AnyPropertyType as xlib::Atom,
                            &mut ty,
                            &mut fmt,
                            &mut n,
                            &mut left,
                            &mut p,
                        );
                        if !p.is_null() {
                            let b = std::slice::from_raw_parts(p, n as usize);
                            if let Some(path) = uri_path(&String::from_utf8_lossy(b)) {
                                wave.load_file(&path);
                            }
                            xlib::XFree(p as _);
                        }
                    }
                    send(src, finished, [ok as c_long, copy as c_long, 0, 0]);
                }
                _ => {}
            }
        }
        xlib::XCloseDisplay(dpy);
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::*;
    use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    pub const API: GuiApiType<'static> = GuiApiType::WIN32;
    pub const EXTS: [&CStr; 2] = [khr::surface::NAME, khr::win32_surface::NAME];

    /// Runs on the host's UI thread. The window's user data holds an `Arc<Wave>` (see `new`/`drop`).
    unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if m != WM_DROPFILES {
            return DefWindowProcW(h, m, w, l);
        }
        let hd = w as HDROP;
        if let Some(wave) = (GetWindowLongPtrW(h, GWLP_USERDATA) as *const Wave).as_ref() {
            // only the first dropped file is looked at
            let mut p = vec![0u16; DragQueryFileW(hd, 0, null_mut(), 0) as usize + 1];
            let n = DragQueryFileW(hd, 0, p.as_mut_ptr(), p.len() as u32) as usize;
            let path = std::ffi::OsString::from_wide(&p[..n]);
            wave.load_file(path.as_ref());
        }
        DragFinish(hd);
        0
    }

    // Raw handles stored as usize so `Native` is Send + Sync (the render thread only needs the HWND).
    pub struct Native {
        hwnd: usize,
        hinst: usize,
    }

    impl Native {
        pub fn new(parent: &Window, wave: Arc<Wave>) -> Result<Self, PluginError> {
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
                wc.lpfnWndProc = Some(wndproc);
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
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::into_raw(wave) as isize);
                DragAcceptFiles(hwnd, 1);
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
            unsafe {
                let wave = GetWindowLongPtrW(self.hwnd as _, GWLP_USERDATA) as *const Wave;
                DestroyWindow(self.hwnd as _);
                drop(Arc::from_raw(wave));
            }
        }
    }
}

pub use imp::*;
