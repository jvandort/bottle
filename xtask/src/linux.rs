//! Linux's distributions, built on Linux: static, with musl, their C built by LinuxC.

use super::*;

/// Linux's distributions, each target's into its directory, built on Linux: static, with musl,
/// their C built by `LinuxC`. One cargo builds the agent for all of them, then one bottle-cli for
/// all of them, so what they share (build scripts, proc macros, for this machine) is built once and
/// the rest in parallel, as cargo sees fit; each's mke2fs and kernel meanwhile, on threads of their
/// own.
pub(super) fn linux_dists(profile: Profile, dists: &[(String, PathBuf)]) -> Result<()> {
    let arches: Vec<&str> = dists
        .iter()
        .map(|(t, _)| t.split('-').next().unwrap())
        .collect();
    let cs = arches
        .iter()
        .map(|arch| LinuxC::new(arch))
        .collect::<Result<Vec<_>>>()?;
    std::thread::scope(|scope| {
        let beside: Vec<_> = dists
            .iter()
            .zip(&cs)
            .zip(&arches)
            .map(|(((triple, out), c), arch)| {
                scope.spawn(move || beside_cli(profile, triple, arch, Some(c), out))
            })
            .collect();
        let built = linux_clis(profile, dists, &arches, &cs);
        let beside = beside
            .into_iter()
            .map(|b| {
                b.join()
                    .map_err(|_| "building mke2fs or the kernel panicked".to_string())?
            })
            .collect::<Result<Vec<()>>>();
        built.and(beside.map(drop))
    })
}

/// Linux's bottle-clis, for each of `dists`' targets, by one cargo, each with its agent built in.
pub(super) fn linux_clis(
    profile: Profile,
    dists: &[(String, PathBuf)],
    arches: &[&str],
    cs: &[LinuxC],
) -> Result<()> {
    let agents = agents(profile, arches)?;
    let triples: Vec<&str> = dists.iter().map(|(t, _)| t.as_str()).collect();
    step(&format!(
        "bottle-cli, for {}, the agent built in",
        triples.join(", ")
    ));
    let mut args = vec!["build"];
    args.extend(profile.packages());
    args.extend(profile.cargo_args());
    let mut env: Vec<(String, OsString)> = vec![];
    for ((triple, c), agent) in triples.iter().zip(cs).zip(&agents) {
        args.extend(["--target", triple]);
        // Its agent; and what cc-rs builds (libkrun's x86_64 kernel decoders, a release's folio's
        // ring) with musl's headers.
        let key = triple.replace('-', "_");
        env.extend([
            (format!("BOTTLE_EMBED_AGENT_{key}"), agent.into()),
            (format!("CC_{key}"), c.gcc.clone().into()),
            (format!("CFLAGS_{key}"), c.specs().into()),
            (format!("AR_{key}"), c.ar.clone().into()),
        ]);
    }
    cargo(&args, &env)?;
    let programs: &[&str] = match profile {
        Profile::Dev => &["bottle-cli"],
        Profile::Release => &["bottle-cli", "folio"],
    };
    for (triple, out) in dists {
        for program in programs {
            copy(
                &target().join(triple).join(profile.dir()).join(program),
                &out.join(program),
            )?;
        }
    }
    Ok(())
}

/// How C is built for a static Linux program for `arch`, on Linux: gcc with musl's specs, which
/// put musl's headers and libc in place of glibc's, as musl-gcc does. For another architecture,
/// Debian's cross gcc, with that architecture's musl (multiarch) and kernel headers.
pub(super) struct LinuxC {
    pub(super) gcc: String,
    pub(super) ar: String,
    pub(super) strip: String,
    /// musl's specs for gcc.
    pub(super) specs: PathBuf,
    /// Where the kernel's headers are: linux/, asm-generic/ and asm/.
    pub(super) headers: [PathBuf; 3],
    /// For another architecture than this machine's: configure's --host.
    pub(super) host: Option<String>,
}

impl LinuxC {
    pub(super) fn new(arch: &str) -> Result<LinuxC> {
        let gnu = format!("{arch}-linux-gnu");
        let debian = match arch {
            "x86_64" => "amd64",
            _ => "arm64",
        };
        let cross = arch != env::consts::ARCH;
        let (gcc, ar, strip, include, asm) = match cross {
            true => (
                format!("{gnu}-gcc"),
                format!("{gnu}-ar"),
                format!("{gnu}-strip"),
                PathBuf::from(format!("/usr/{gnu}/include")),
                PathBuf::from(format!("/usr/{gnu}/include/asm")),
            ),
            false => (
                "gcc".into(),
                "ar".into(),
                "strip".into(),
                PathBuf::from("/usr/include"),
                PathBuf::from(format!("/usr/include/{gnu}/asm")),
            ),
        };
        let specs = PathBuf::from(format!("/usr/lib/{arch}-linux-musl/musl-gcc.specs"));
        if !specs.exists() {
            return Err(match cross {
                true => format!(
                    "no musl for {arch} ({}): on Debian, dpkg --add-architecture {debian}, then \
                     apt install gcc-{}-linux-gnu musl-dev:{debian} linux-libc-dev-{debian}-cross",
                    specs.display(),
                    arch.replace('_', "-")
                ),
                false => format!(
                    "no musl ({}): on Debian, apt install musl-tools",
                    specs.display()
                ),
            });
        }
        Ok(LinuxC {
            gcc,
            ar,
            strip,
            specs,
            headers: [include.join("linux"), include.join("asm-generic"), asm],
            host: cross.then(|| format!("{arch}-linux-musl")),
        })
    }

    pub(super) fn specs(&self) -> String {
        format!("-specs {}", self.specs.display())
    }

    /// gcc as configure takes it: with musl's specs.
    pub(super) fn cc(&self) -> String {
        format!("{} {}", self.gcc, self.specs())
    }
}
