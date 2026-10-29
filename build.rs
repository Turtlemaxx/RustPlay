fn main() {
    // FairPlay key decryption (playfair, GPL-3.0) is compiled from the vendored C sources.
    cc::Build::new()
        .files([
            "vendor/playfair/playfair.c",
            "vendor/playfair/omg_hax.c",
            "vendor/playfair/hand_garble.c",
            "vendor/playfair/modified_md5.c",
            "vendor/playfair/sap_hash.c",
        ])
        .include("vendor/playfair")
        .warnings(false)
        .compile("playfair");
    println!("cargo:rerun-if-changed=vendor/playfair");
    // The app icon is embedded at build time.
    println!("cargo:rerun-if-changed=icons");

    tauri_build::build();
}
