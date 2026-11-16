//! libkrun, safely: its C API, behind methods that take what each call reads as Rust has it (C
//! strings borrowed for the call, which libkrun copies; memory it serves the VM from for as long as
//! the VM runs as `'static`), and give back its errors as io::Error. The only `unsafe` for libkrun
//! is here.

use std::ffi::{CStr, c_char};
use std::io;

/// What libkrun reads of an array of strings, NULL-terminated or not: MAX_ARGS pointers (it makes
/// a slice of that many, then stops at the first NULL).
const MAX_ARGS: usize = 4096;

/// A VM being configured: a libkrun context.
pub struct Vm(u32);

/// `ret`, a libkrun return value, as a result: what failed, and its errno.
fn check(what: &str, ret: i32) -> io::Result<i32> {
    if ret < 0 {
        Err(io::Error::other(format!(
            "{what}: {}",
            io::Error::from_raw_os_error(-ret)
        )))
    } else {
        Ok(ret)
    }
}

/// Pointers to `strings`, then NULLs, as many as libkrun reads of an array of strings.
fn string_array(strings: &[&CStr]) -> io::Result<Vec<*const c_char>> {
    if strings.len() >= MAX_ARGS {
        return Err(io::Error::other(format!(
            "{} strings, more than libkrun takes ({})",
            strings.len(),
            MAX_ARGS - 1
        )));
    }
    let mut array: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    array.resize(MAX_ARGS, std::ptr::null());
    Ok(array)
}

/// libkrun's log level, from 0 (off) to 5 (trace), for every VM.
pub fn set_log_level(level: u32) {
    krun::krun_set_log_level(level);
}

/// The most vCPUs the hypervisor gives a VM; None if it won't say.
pub fn max_vcpus() -> Option<u8> {
    u8::try_from(krun::krun_get_max_vcpus())
        .ok()
        .filter(|&n| n > 0)
}

impl Vm {
    pub fn new() -> io::Result<Vm> {
        Ok(Vm(
            check("creating libkrun's context", krun::krun_create_ctx())? as u32,
        ))
    }

    pub fn set_vm_config(&self, cpus: u8, mem_mib: u32) -> io::Result<()> {
        check(
            "setting cpus and memory",
            krun::krun_set_vm_config(self.0, cpus, mem_mib),
        )?;
        Ok(())
    }

    pub fn set_free_page_reporting_mode(&self, mode: u32) -> io::Result<()> {
        check(
            "setting free page reporting",
            krun::krun_set_balloon_free_page_reporting_mode(self.0, mode),
        )?;
        Ok(())
    }

    pub fn disable_implicit_vsock(&self) -> io::Result<()> {
        check(
            "disabling the implicit vsock",
            krun::krun_disable_implicit_vsock(self.0),
        )?;
        Ok(())
    }

    pub fn add_vsock(&self, tsi_features: u32) -> io::Result<()> {
        check("adding vsock", krun::krun_add_vsock(self.0, tsi_features))?;
        Ok(())
    }

    /// Vsock `port`, carried to the unix socket at `path`: listened on there, or connected to.
    pub fn add_vsock_port(
        &self,
        what: &str,
        port: u32,
        path: &CStr,
        listen: bool,
    ) -> io::Result<()> {
        // SAFETY: a C string, which libkrun copies.
        check(what, unsafe {
            krun::krun_add_vsock_port2(self.0, port, path.as_ptr(), listen)
        })?;
        Ok(())
    }

    /// A virtio-fs device tagged `tag`, of the host's directory `path` (None: none, only what's
    /// added to it).
    pub fn add_virtiofs(
        &self,
        what: &str,
        tag: &CStr,
        path: Option<&CStr>,
        read_only: bool,
    ) -> io::Result<()> {
        let path = path.map_or(std::ptr::null(), CStr::as_ptr);
        // SAFETY: C strings, or NULL for the path, which libkrun copies.
        check(what, unsafe {
            krun::krun_add_virtiofs3(self.0, tag.as_ptr(), path, 0, read_only)
        })?;
        Ok(())
    }

    /// A directory at `path` in the virtio-fs device tagged `tag`.
    pub fn add_overlay_dir(
        &self,
        what: &str,
        tag: &CStr,
        path: &CStr,
        mode: u32,
    ) -> io::Result<()> {
        // SAFETY: C strings, which libkrun copies.
        check(what, unsafe {
            krun::krun_fs_add_overlay_dir(self.0, tag.as_ptr(), path.as_ptr(), mode)
        })?;
        Ok(())
    }

    /// A file at `path` in the virtio-fs device tagged `tag`, served from `data` for as long as
    /// the VM runs.
    pub fn add_overlay_file(
        &self,
        what: &str,
        tag: &CStr,
        path: &CStr,
        data: &'static [u8],
        mode: u32,
    ) -> io::Result<()> {
        // SAFETY: C strings, which libkrun copies; data it keeps, which lives as long as anything.
        check(what, unsafe {
            krun::krun_fs_add_overlay_file(
                self.0,
                tag.as_ptr(),
                path.as_ptr(),
                data.as_ptr(),
                data.len(),
                mode,
                false,
            )
        })?;
        Ok(())
    }

    pub fn add_disk(
        &self,
        what: &str,
        id: &CStr,
        path: &CStr,
        format: u32,
        read_only: bool,
    ) -> io::Result<()> {
        // SAFETY: C strings, which libkrun copies.
        check(what, unsafe {
            krun::krun_add_disk2(self.0, id.as_ptr(), path.as_ptr(), format, read_only)
        })?;
        Ok(())
    }

    /// What the VM runs: `program`, with `argv` (its arguments) and `envp`.
    pub fn set_exec(&self, program: &CStr, argv: &[&CStr], envp: &[&CStr]) -> io::Result<()> {
        let (argv, envp) = (string_array(argv)?, string_array(envp)?);
        // SAFETY: a C string, and arrays of them as long as libkrun reads, NULL after the last;
        // libkrun copies them all.
        check("setting what the VM runs", unsafe {
            krun::krun_set_exec(self.0, program.as_ptr(), argv.as_ptr(), envp.as_ptr())
        })?;
        Ok(())
    }

    pub fn set_console_output(&self, path: &CStr) -> io::Result<()> {
        // SAFETY: a C string, which libkrun copies.
        check("setting the console", unsafe {
            krun::krun_set_console_output(self.0, path.as_ptr())
        })?;
        Ok(())
    }

    /// The kernel the VM boots, at `path`, in `format`; no initramfs, and its own command line.
    pub fn set_kernel(&self, path: &CStr, format: u32) -> io::Result<()> {
        // SAFETY: a C string, which libkrun copies, and NULLs.
        check("setting the kernel", unsafe {
            krun::krun_set_kernel(
                self.0,
                path.as_ptr(),
                format,
                std::ptr::null(),
                std::ptr::null(),
            )
        })?;
        Ok(())
    }

    /// Starts the VM, in this process: returns only if it couldn't; once it has, its end is this
    /// process's.
    pub fn start_enter(self) -> io::Error {
        match check("starting the VM", krun::krun_start_enter(self.0)) {
            Err(e) => e,
            Ok(_) => io::Error::other("libkrun returned from starting the VM"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_array_is_as_long_as_libkrun_reads() {
        let array = string_array(&[c"a", c"b"]).unwrap();
        assert_eq!(array.len(), MAX_ARGS);
        assert!(!array[1].is_null() && array[2..].iter().all(|p| p.is_null()));
        assert!(string_array(&vec![c"a"; MAX_ARGS]).is_err());
    }
}
