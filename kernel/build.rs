fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");

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
