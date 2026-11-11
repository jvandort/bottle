//! libkrun 1.x, loaded when it's wanted rather than linked: bottle-cli builds and runs its other
//! commands without it. Only what `boot` calls.

use std::ffi::{CStr, CString, c_char, c_void};
use std::io;
use std::path::{Path, PathBuf};

pub const DISK_FORMAT_RAW: u32 = 0;
pub const FREE_PAGE_REPORTING_MODE_REPLACE: u32 = 1;
/// The tag of the root filesystem's virtio-fs device.
pub const ROOT_TAG: &CStr = c"/dev/root";

type CtxFn = unsafe extern "C" fn(u32) -> i32;

/// libkrun's functions, each taking the context first and returning 0 or a negative errno.
pub struct Krun {
    pub set_log_level: unsafe extern "C" fn(u32) -> i32,
    pub create_ctx: unsafe extern "C" fn() -> i32,
    pub set_vm_config: unsafe extern "C" fn(u32, u8, u32) -> i32,
    pub disable_implicit_vsock: CtxFn,
    pub add_vsock: unsafe extern "C" fn(u32, u32) -> i32,
    pub add_vsock_port2: unsafe extern "C" fn(u32, u32, *const c_char, bool) -> i32,
    pub add_virtiofs3: unsafe extern "C" fn(u32, *const c_char, *const c_char, u64, bool) -> i32,
    pub fs_add_overlay_dir: unsafe extern "C" fn(u32, *const c_char, *const c_char, u32) -> i32,
    pub fs_add_overlay_file:
        unsafe extern "C" fn(u32, *const c_char, *const c_char, *const u8, usize, u32, bool) -> i32,
    pub add_disk2: unsafe extern "C" fn(u32, *const c_char, *const c_char, u32, bool) -> i32,
    pub set_balloon_free_page_reporting_mode: unsafe extern "C" fn(u32, u32) -> i32,
    pub set_exec:
        unsafe extern "C" fn(u32, *const c_char, *const *const c_char, *const *const c_char) -> i32,
    pub set_console_output: unsafe extern "C" fn(u32, *const c_char) -> i32,
    pub start_enter: CtxFn,
}

/// The library's file name, as the platform has it.
#[cfg(target_os = "macos")]
const NAME: &str = "libkrun.dylib";
#[cfg(not(target_os = "macos"))]
const NAME: &str = "libkrun.so.1";

/// Where libkrun is looked for: $BOTTLE_LIBKRUN, else beside this program, else wherever the
/// dynamic loader looks.
pub fn locate() -> PathBuf {
    if let Some(path) = std::env::var_os("BOTTLE_LIBKRUN") {
        return path.into();
    }
    if let Ok(exe) = std::env::current_exe() {
        let beside = exe.with_file_name(NAME);
        if beside.exists() {
            return beside;
        }
    }
    NAME.into()
}

impl Krun {
    // Each transmute's type is its field's: the annotations would say it all again.
    #[allow(clippy::missing_transmute_annotations)]
    pub fn load(path: &Path) -> io::Result<Krun> {
        let name = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::other("a NUL in libkrun's path"))?;
        // SAFETY: dlopen with a C string; the handle is never closed, the functions living as long
        // as this process.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(io::Error::other(format!(
                "loading {}: {}",
                path.display(),
                dlerror()
            )));
        }
        let sym = |name: &CStr| -> io::Result<*mut c_void> {
            // SAFETY: dlsym on the handle just opened.
            let f = unsafe { libc::dlsym(handle, name.as_ptr()) };
            if f.is_null() {
                Err(io::Error::other(format!(
                    "{}: no {} (libkrun 1.19 or later is needed)",
                    path.display(),
                    name.to_string_lossy()
                )))
            } else {
                Ok(f)
            }
        };
        // SAFETY: each symbol is libkrun's function of that name, whose C signature (libkrun.h)
        // the field's type is.
        unsafe {
            Ok(Krun {
                set_log_level: std::mem::transmute(sym(c"krun_set_log_level")?),
                create_ctx: std::mem::transmute(sym(c"krun_create_ctx")?),
                set_vm_config: std::mem::transmute(sym(c"krun_set_vm_config")?),
                disable_implicit_vsock: std::mem::transmute(sym(c"krun_disable_implicit_vsock")?),
                add_vsock: std::mem::transmute(sym(c"krun_add_vsock")?),
                add_vsock_port2: std::mem::transmute(sym(c"krun_add_vsock_port2")?),
                add_virtiofs3: std::mem::transmute(sym(c"krun_add_virtiofs3")?),
                fs_add_overlay_dir: std::mem::transmute(sym(c"krun_fs_add_overlay_dir")?),
                fs_add_overlay_file: std::mem::transmute(sym(c"krun_fs_add_overlay_file")?),
                add_disk2: std::mem::transmute(sym(c"krun_add_disk2")?),
                set_balloon_free_page_reporting_mode: std::mem::transmute(sym(
                    c"krun_set_balloon_free_page_reporting_mode",
                )?),
                set_exec: std::mem::transmute(sym(c"krun_set_exec")?),
                set_console_output: std::mem::transmute(sym(c"krun_set_console_output")?),
                start_enter: std::mem::transmute(sym(c"krun_start_enter")?),
            })
        }
    }
}

fn dlerror() -> String {
    // SAFETY: dlerror returns a C string or NULL.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        return "unknown error".into();
    }
    // SAFETY: a C string dlerror just returned.
    unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
}

/// `ret`, a libkrun return value, as a result: what failed, and its errno.
pub fn check(what: &str, ret: i32) -> io::Result<i32> {
    if ret < 0 {
        Err(io::Error::other(format!(
            "{what}: {}",
            io::Error::from_raw_os_error(-ret)
        )))
    } else {
        Ok(ret)
    }
}
