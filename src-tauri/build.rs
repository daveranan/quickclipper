fn main() {
  // The ScreenCaptureKit bridge is written in Swift. Link arguments from a
  // dependency's build script do not reach this binary, so point it at the
  // system Swift runtime here.
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
  }
  tauri_build::build()
}
