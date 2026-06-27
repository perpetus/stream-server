fn main() {
    // Pin the "fluent" base style so the few std-widgets we still use (ScrollView,
    // TextInput selection, etc.) render consistently across platforms regardless of
    // the system default.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".to_string());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile Slint UI");
}
