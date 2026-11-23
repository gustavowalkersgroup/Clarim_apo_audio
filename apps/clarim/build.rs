fn main() {
    // Static imports resolve from System32 only (no DLL planting next to the exe). Linker flag, MSVC only.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
}
