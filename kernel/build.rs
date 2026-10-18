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

/// The vDSO of 64-bit programs (vdso/vdso.c): a small shared object the kernel embeds and
/// maps into each process (src/vdso.rs). User-mode code, so built like a shared library,
/// with no relocations and Linux's symbol versions (vdso/vdso.lds).
fn build_vdso(dir: &str) {
    let out = std::env::var("OUT_DIR").unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());
    for f in ["vdso.c", "vdso.lds"] {
        println!("cargo:rerun-if-changed={dir}/vdso/{f}");
    }
    let status = std::process::Command::new(&cc)
        .args(["-O2", "-fPIC", "-fno-stack-protector", "-fno-asynchronous-unwind-tables", "-fcf-protection=none", "-nostdlib", "-shared"])
        .arg(format!("-Wl,-T,{dir}/vdso/vdso.lds"))
        .args(["-Wl,--hash-style=both", "-Wl,-soname=linux-vdso.so.1", "-Wl,--no-undefined", "-Wl,--build-id=none", "-Wl,-z,max-page-size=4096"])
        .arg("-o")
        .arg(format!("{out}/vdso.so"))
        .arg(format!("{dir}/vdso/vdso.c"))
        .status()
        .expect("running the C compiler for the vDSO");
    assert!(status.success(), "building the vDSO failed");
}
