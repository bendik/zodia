fn main() {
    glib_build_tools::compile_resources(
        &["icons"],
        "icons/icons.gresource.xml",
        "icons.gresource",
    );
}
