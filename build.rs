fn main() {
    println!("cargo:rerun-if-changed=assets/icons/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icons/app.ico")
            .set("ProductName", "PC Activity Scanner")
            .set("FileDescription", "PC Activity Scanner")
            .compile()
            .expect("Failed to embed Windows application icon");
    }
}
