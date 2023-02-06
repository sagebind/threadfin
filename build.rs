use std::env;

fn main() {
    let target = env::var("TARGET").unwrap();

    if target.starts_with("x86_64")
        || target.starts_with("i686")
        || target.starts_with("aarch64")
        || target.starts_with("powerpc64")
        || target.starts_with("sparc64")
        || target.starts_with("mips64el")
        || target.starts_with("riscv64")
    {
        println!("cargo:rustc-cfg=threadfin_has_atomic64");
    }
}
