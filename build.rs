use std::{env, path::PathBuf, process::Command};

fn main() {
    let manifest = "packaging/potik.gresource.xml";
    println!("cargo::rerun-if-changed={manifest}");
    println!("cargo::rerun-if-changed=packaging/io.github.potik.Downloads.png");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let status = Command::new("glib-compile-resources")
        .args(["--sourcedir", "packaging", "--target"])
        .arg(output.join("potik.gresource"))
        .arg(manifest)
        .status()
        .expect("glib-compile-resources is required to build Potik");
    assert!(status.success(), "Could not compile Potik's GTK resources");
}
