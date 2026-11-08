fn main() {
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    println!("cargo:rustc-link-arg-bin=delta=/STACK:8388608");
}
