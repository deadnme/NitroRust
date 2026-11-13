fn main() {
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_required().unwrap();
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
}
