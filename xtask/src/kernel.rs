//! The kernel a distribution's VMs boot: libkrunfw's, out of its release, as an Image (arm64) or an ELF (x86_64).

use std::fs;
use std::path::Path;
use std::process::Command;

use super::{copy, fetch, run, step};

use super::Result;

/// libkrunfw's release the VMs boot, for each architecture: where it is, and its sha256.
pub(super) const KERNEL_AARCH64: (&str, &str) = (
    "https://github.com/containers/libkrunfw/releases/download/v5.6.2/libkrunfw-prebuilt-aarch64.tgz",
    "adef6739cc3bda2a1b57f04c08ccbfdf206a6c831d76b32de1beddc0e7984e86",
);
pub(super) const KERNEL_X86_64: (&str, &str) = (
    "https://kojipkgs.fedoraproject.org/packages/libkrunfw/5.6.2/1.fc43/x86_64/libkrunfw-5.6.2-1.fc43.x86_64.rpm",
    "484f811b7a476040a2fc8d48df3d8daddced3d76cc49cca5889e0beb7d913c60",
);

/// Where libkrunfw 5.6.2's x86_64 kernel goes in guest memory, where it starts, and its size: what
/// its `krunfw_get_kernel` says (by disassembly of Fedora's build, whose sha256 is pinned above).
pub(super) const KERNEL_X86_64_LOAD: u64 = 0x100_0000;
pub(super) const KERNEL_X86_64_ENTRY: u64 = 0x100_0123;
pub(super) const KERNEL_X86_64_SIZE: usize = 0x16b_0000;

/// The kernel for `arch`, into `out`, by way of `cache`.
pub(super) fn kernel(arch: &str, cache: &Path, out: &Path) -> Result<()> {
    fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    // What an older dist had beside bottle-cli, which libkrun loaded.
    let _ = fs::remove_file(out.join("libkrunfw.so.5"));
    match arch {
        "aarch64" => {
            let image = cache.join("kernel-5.6.2-aarch64");
            if !image.exists() {
                step("the kernel, libkrunfw 5.6.2's");
                let bundle = fetch(cache, KERNEL_AARCH64)?;
                run(Command::new("tar")
                    .args(["-xzf"])
                    .arg(&bundle)
                    .arg("-C")
                    .arg(cache)
                    .arg("libkrunfw/kernel.c"))?;
                let c = fs::read(cache.join("libkrunfw/kernel.c")).map_err(|e| e.to_string())?;
                let partial = image.with_extension("partial");
                fs::write(&partial, decode_bundle(&c)?).map_err(|e| e.to_string())?;
                fs::rename(&partial, &image).map_err(|e| e.to_string())?;
                let _ = fs::remove_dir_all(cache.join("libkrunfw"));
            }
            copy(&image, &out.join("kernel")).map(drop)
        }
        "x86_64" => {
            let library = cache.join("libkrunfw.so.5.6.2");
            if !library.exists() {
                step("the kernel, libkrunfw 5.6.2's, Fedora's build");
                let rpm = fetch(cache, KERNEL_X86_64)?;
                // An rpm is a cpio, which bsdtar reads, or rpm2cpio turns into one.
                let extract = format!(
                    "cd {dir} && (bsdtar -xf {rpm} ./usr/lib64/libkrunfw.so.5.6.2 2>/dev/null || \
                     rpm2cpio {rpm} | cpio -id ./usr/lib64/libkrunfw.so.5.6.2) && \
                     mv usr/lib64/libkrunfw.so.5.6.2 . && rm -rf usr",
                    dir = cache.display(),
                    rpm = rpm.display()
                );
                run(Command::new("sh").arg("-c").arg(&extract))
                    .map_err(|e| format!("{e} (bsdtar, or rpm2cpio and cpio, is needed)"))?;
            }
            let image = cache.join("kernel-5.6.2-x86_64");
            if !image.exists() {
                let so = fs::read(&library).map_err(|e| e.to_string())?;
                let bundle = elf_symbol(&so, "KERNEL_BUNDLE")?;
                // A C string: the kernel, then a NUL.
                if bundle.len() != KERNEL_X86_64_SIZE + 1 {
                    return Err(format!(
                        "libkrunfw's KERNEL_BUNDLE is {:#x} bytes, not {:#x}",
                        bundle.len(),
                        KERNEL_X86_64_SIZE + 1
                    ));
                }
                let elf = elf_kernel(
                    &bundle[..KERNEL_X86_64_SIZE],
                    KERNEL_X86_64_LOAD,
                    KERNEL_X86_64_ENTRY,
                );
                let partial = image.with_extension("partial");
                fs::write(&partial, elf).map_err(|e| e.to_string())?;
                fs::rename(&partial, &image).map_err(|e| e.to_string())?;
            }
            copy(&image, &out.join("kernel")).map(drop)
        }
        arch => Err(format!("no kernel for {arch}")),
    }
}

/// The bytes of `name`, a symbol in the dynamic symbol table of ELF64 shared library `so`.
pub(super) fn elf_symbol<'a>(so: &'a [u8], name: &str) -> Result<&'a [u8]> {
    let bad = || format!("not an ELF64 library with {name}");
    let at = |offset: u64, size: usize| -> Result<u64> {
        let bytes = so
            .get(offset as usize..offset as usize + size)
            .ok_or_else(bad)?;
        let mut value = [0u8; 8];
        value[..size].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(value))
    };
    if so.get(..6) != Some(b"\x7fELF\x02\x01") {
        return Err(bad());
    }
    let (phoff, phentsize, phnum) = (at(0x20, 8)?, at(0x36, 2)?, at(0x38, 2)?);
    let (shoff, shentsize, shnum) = (at(0x28, 8)?, at(0x3a, 2)?, at(0x3c, 2)?);
    let section = |i: u64| shoff + i * shentsize;
    // SHT_DYNSYM, and the string table it links to.
    let dynsym = (0..shnum)
        .map(section)
        .find(|&s| at(s + 4, 4).ok() == Some(11))
        .ok_or_else(bad)?;
    let strtab = at(section(at(dynsym + 0x28, 4)?) + 0x18, 8)?;
    let (symbols, size, entsize) = (
        at(dynsym + 0x18, 8)?,
        at(dynsym + 0x20, 8)?,
        at(dynsym + 0x38, 8)?,
    );
    for symbol in (symbols..symbols + size).step_by(entsize.max(1) as usize) {
        let start = (strtab + at(symbol, 4)?) as usize;
        let end = start + so[start..].iter().position(|&b| b == 0).ok_or_else(bad)?;
        if &so[start..end] != name.as_bytes() {
            continue;
        }
        let (value, length) = (at(symbol + 8, 8)?, at(symbol + 16, 8)?);
        // Where it is in the file: by the PT_LOAD segment that holds it.
        for header in (0..phnum).map(|i| phoff + i * phentsize) {
            let (offset, vaddr, filesz) = (
                at(header + 8, 8)?,
                at(header + 0x10, 8)?,
                at(header + 0x20, 8)?,
            );
            if at(header, 4)? == 1 && vaddr <= value && value + length <= vaddr + filesz {
                let start = (offset + value - vaddr) as usize;
                return so.get(start..start + length as usize).ok_or_else(bad);
            }
        }
    }
    Err(bad())
}

/// An x86_64 ELF executable of `image`, loaded at physical address `load` and entered at `entry`:
/// how libkrun takes an x86_64 kernel from a file (krun_set_kernel's ELF format, which loads each
/// PT_LOAD segment at its physical address and starts at the entry, as it does libkrunfw's).
pub(super) fn elf_kernel(image: &[u8], load: u64, entry: u64) -> Vec<u8> {
    const PAGE: u64 = 0x1000;
    let mut elf = Vec::with_capacity(PAGE as usize + image.len());
    // The ELF header: 64-bit, little-endian, version 1; an executable for x86-64.
    elf.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    for (value, size) in [
        (2, 2),     // e_type: ET_EXEC
        (62, 2),    // e_machine: EM_X86_64
        (1, 4),     // e_version
        (entry, 8), // e_entry
        (64, 8),    // e_phoff: right after this header
        (0, 8),     // e_shoff: no sections
        (0, 4),     // e_flags
        (64, 2),    // e_ehsize
        (56, 2),    // e_phentsize
        (1, 2),     // e_phnum
        (64, 2),    // e_shentsize
        (0, 2),     // e_shnum
        (0, 2),     // e_shstrndx
        // The one program header: the image, read, written and run.
        (1, 4),                  // p_type: PT_LOAD
        (7, 4),                  // p_flags: RWX
        (PAGE, 8),               // p_offset
        (load, 8),               // p_vaddr
        (load, 8),               // p_paddr
        (image.len() as u64, 8), // p_filesz
        (image.len() as u64, 8), // p_memsz
        (PAGE, 8),               // p_align
    ] {
        elf.extend_from_slice(&u64::to_le_bytes(value)[..size]);
    }
    elf.resize(PAGE as usize, 0);
    elf.extend_from_slice(image);
    elf
}

/// The bytes of libkrunfw's kernel.c's KERNEL_BUNDLE: a C string of nothing but \x escapes, from
/// its `KERNEL_BUNDLE[] =` to its `;`.
pub(super) fn decode_bundle(c: &[u8]) -> Result<Vec<u8>> {
    let start = find(c, b"KERNEL_BUNDLE[] =").ok_or("kernel.c: no KERNEL_BUNDLE")?;
    let mut out = Vec::with_capacity(c.len() / 4);
    let mut i = start;
    let mut quoted = false;
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    while i < c.len() {
        match (quoted, c[i]) {
            (false, b';') => return Ok(out),
            (false, b'"') | (true, b'"') => quoted = !quoted,
            (true, b'\\') if c.get(i + 1) == Some(&b'x') => {
                let mut value = 0u8;
                let mut digits = 0;
                while digits < 2
                    && let Some(d) = c.get(i + 2 + digits).copied().and_then(hex)
                {
                    value = value * 16 + d;
                    digits += 1;
                }
                if digits == 0 {
                    return Err("kernel.c: a \\x with no digits".into());
                }
                out.push(value);
                i += 1 + digits;
            }
            (true, b) => out.push(b),
            (false, _) => {}
        }
        i += 1;
    }
    Err("kernel.c: KERNEL_BUNDLE doesn't end".into())
}

pub(super) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
