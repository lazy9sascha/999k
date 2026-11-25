#[path = "src/cloud/registration_origin.rs"]
mod registration_origin;

fn main() {
    const SETTING: &str = "RADIO999K_RELEASE_REGISTRATION_URL";
    println!("cargo:rerun-if-env-changed={SETTING}");
    println!("cargo:rerun-if-changed=src/cloud/registration_origin.rs");
    println!("cargo:rerun-if-changed=build.rs");
    let configured = std::env::var(SETTING).unwrap_or_else(|err| match err {
        std::env::VarError::NotPresent => String::new(),
        _ => panic!("{SETTING} must contain a public HTTPS origin"),
    });
    let origin = if configured.is_empty() {
        String::new()
    } else {
        registration_origin::https_origin(&configured)
            .expect("invalid release registration origin")
            .to_string()
    };
    println!("cargo:rustc-env=RADIO999K_COMPILED_REGISTRATION_ORIGIN={origin}");
}
