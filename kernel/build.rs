fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");

    build_uacpi(&dir);
    build_vdso(&dir);

    // FEATURES=linux-drivers: link the Linux objects scripts/build-linux.sh built. They are
    // passed as plain object files (not an archive), so the drivers, which nothing
    // references by name, are linked too; their initcalls are KEPT by linker.ld.
    if std::env::var_os("CARGO_FEATURE_LINUX_DRIVERS").is_some() {
        let linux = format!("{dir}/../build/linux");
        let list = format!("{linux}/nova-objects.list");
        println!("cargo:rerun-if-changed={list}");
        let objects = std::fs::read_to_string(&list)
            .unwrap_or_else(|_| panic!("{list} is missing: run scripts/fetch-linux.sh, then build with FEATURES=linux-drivers scripts/build.sh"));
        for object in objects.lines().filter(|l| !l.is_empty()) {
            println!("cargo:rustc-link-arg={object}");
            println!("cargo:rerun-if-changed={object}");
        }
        let version = std::fs::read_to_string(format!("{linux}/VERSION")).unwrap_or_default();
        println!("cargo:rustc-env=NOVA_LINUX_VERSION={}", version.trim());
    }
}

/// uACPI (third_party/uacpi, MIT) and Nova's glue for it (acpi/nova_acpi.c): freestanding
/// C for the kernel's code model, linked as a static library. The Rust side is src/acpi.rs.
fn build_uacpi(dir: &str) {
    let out = std::env::var("OUT_DIR").unwrap();
    let uacpi = format!("{dir}/third_party/uacpi");
    let mut sources: Vec<String> = std::fs::read_dir(format!("{uacpi}/source"))
        .expect("third_party/uacpi/source")
        .filter_map(|e| e.ok().map(|e| e.path().display().to_string()))
        .filter(|p| p.ends_with(".c"))
        .collect();
    sources.sort();
    sources.push(format!("{dir}/acpi/nova_acpi.c"));
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());
    let mut objects = Vec::new();
    for src in &sources {
        let obj = format!("{out}/{}.o", std::path::Path::new(src).file_stem().unwrap().to_string_lossy());
        let status = std::process::Command::new(&cc)
            .args(["-c", "-O2", "-std=gnu11", "-ffreestanding", "-fno-stack-protector", "-fno-pic", "-fno-pie"])
            .args(["-mcmodel=kernel", "-mno-red-zone", "-mgeneral-regs-only", "-fno-asynchronous-unwind-tables"])
            .args(["-fno-strict-aliasing", "-Wall", "-Wno-unused-function", "-DUACPI_SIZED_FREES", "-DUACPI_NATIVE_ALLOC_ZEROED"])
            .arg(format!("-I{uacpi}/include"))
            .args([src.as_str(), "-o", obj.as_str()])
            .status()
            .expect("running the C compiler for uACPI");
        assert!(status.success(), "compiling {src} failed");
        println!("cargo:rerun-if-changed={src}");
        objects.push(obj);
    }
    let lib = format!("{out}/libuacpi.a");
    let _ = std::fs::remove_file(&lib);
    let status = std::process::Command::new("ar").arg("rcs").arg(&lib).args(&objects).status().expect("running ar");
    assert!(status.success(), "ar failed for uACPI");
    println!("cargo:rerun-if-changed={uacpi}/include");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=uacpi");
    let version = std::fs::read_to_string(format!("{uacpi}/VERSION")).unwrap_or_default();
    println!("cargo:rustc-env=NOVA_UACPI_VERSION={}", version.trim());
}

/// The vDSOs of 64- and 32-bit programs (vdso/vdso.c, vdso/vdso32.c): small shared objects
/// the kernel embeds and maps into each process (src/vdso.rs). User-mode code, so built
/// like shared libraries, with no relocations and Linux's symbol versions (vdso/*.lds).
fn build_vdso(dir: &str) {
    let out = std::env::var("OUT_DIR").unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());
    for (name, arch, soname) in [("vdso", "-m64", "linux-vdso.so.1"), ("vdso32", "-m32", "linux-gate.so.1")] {
        for ext in ["c", "lds"] {
            println!("cargo:rerun-if-changed={dir}/vdso/{name}.{ext}");
        }
        let status = std::process::Command::new(&cc)
            .args([arch, "-O2", "-fPIC", "-fno-stack-protector", "-fno-asynchronous-unwind-tables", "-fcf-protection=none", "-nostdlib", "-shared"])
            .arg(format!("-Wl,-T,{dir}/vdso/{name}.lds"))
            .arg(format!("-Wl,-soname={soname}"))
            .args(["-Wl,--hash-style=both", "-Wl,--no-undefined", "-Wl,--build-id=none", "-Wl,-z,max-page-size=4096"])
            .arg("-o")
            .arg(format!("{out}/{name}.so"))
            .arg(format!("{dir}/vdso/{name}.c"))
            .status()
            .expect("running the C compiler for the vDSO");
        assert!(status.success(), "building {name}.so failed (the 32-bit one needs gcc's -m32 support)");
    }
    // AT_SYSINFO of 32-bit programs: where __kernel_vsyscall is in vdso32.so.
    let syms = std::process::Command::new("nm").arg("-D").arg(format!("{out}/vdso32.so")).output().expect("running nm on vdso32.so");
    let syms = String::from_utf8_lossy(&syms.stdout);
    let offset = syms
        .lines()
        .find(|l| l.ends_with(" __kernel_vsyscall") || l.contains(" __kernel_vsyscall@"))
        .and_then(|l| u64::from_str_radix(l.split_whitespace().next()?, 16).ok())
        .expect("__kernel_vsyscall in vdso32.so");
    println!("cargo:rustc-env=NOVA_VSYSCALL32={offset}");
}
