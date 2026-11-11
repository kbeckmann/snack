// On macOS, compile the Secure Enclave password helper (macos/sealed_password.swift)
// into a static library and link it with the Swift runtime. Other platforms
// build nothing here.
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=macos/sealed_password.swift");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") { return; }

    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => panic!("unsupported macOS architecture: {other:?}"),
    };
    // CryptoKit's Secure Enclave keys need macOS 10.15; snack.app targets 11.
    let target = format!("{arch}-apple-macosx11.0");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let library = out.join("libsnack_sealed_password.a");

    let status = Command::new("xcrun")
        .args(["swiftc", "-parse-as-library", "-emit-library", "-static", "-O",
            "-module-name", "SnackSealedPassword", "-target", &target, "-o"])
        .arg(&library)
        .arg("macos/sealed_password.swift")
        .status()
        .expect("running swiftc (install the Xcode command line tools)");
    assert!(status.success(), "swiftc failed to build macos/sealed_password.swift");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=snack_sealed_password");

    // The Swift runtime ships with macOS; link against the SDK and toolchain
    // stubs and load it from /usr/lib/swift at run time.
    let info = Command::new("xcrun").args(["swiftc", "-print-target-info", "-target", &target])
        .output().expect("querying swiftc target info");
    let info = String::from_utf8_lossy(&info.stdout);
    for path in json_strings(&info, "runtimeLibraryPaths") {
        println!("cargo:rustc-link-search=native={path}");
    }
    let sdk = Command::new("xcrun").args(["--show-sdk-path"]).output().expect("locating the macOS SDK");
    let sdk = String::from_utf8_lossy(&sdk.stdout).trim().to_string();
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for framework in ["CryptoKit", "LocalAuthentication", "Security", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

// The string array under `key` in swiftc's JSON target info.
fn json_strings(json: &str, key: &str) -> Vec<String> {
    let Some(start) = json.find(&format!("\"{key}\"")) else { return Vec::new() };
    let Some(open) = json[start..].find('[') else { return Vec::new() };
    let Some(close) = json[start + open..].find(']') else { return Vec::new() };
    json[start + open + 1..start + open + close]
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}
