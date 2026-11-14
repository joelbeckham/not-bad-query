fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/LICENSE-lucide");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI's Windows backend loads icon resource 1 for its window class.
        embed_resource::compile_for("assets/app.rc", ["not-bad-query"], embed_resource::NONE)
            .manifest_required()
            .expect("compile Windows application icon resources");
    }
}
