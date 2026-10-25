// Native look per platform: Cupertino on macOS, Fluent elsewhere (Windows).
fn main() {
    let style = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") { "cupertino" } else { "fluent" };
    let config = slint_build::CompilerConfiguration::new().with_style(style.into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint compile");
}
