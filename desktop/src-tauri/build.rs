fn main() {
    println!("cargo:rustc-env=SWIFTCACHE_TARGET={}", std::env::var("TARGET").unwrap());
    tauri_build::build();
}
