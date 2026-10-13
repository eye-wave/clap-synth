//! Win32: child window + WM_DROPFILES.
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
