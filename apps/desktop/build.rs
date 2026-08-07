fn main() {
    #[cfg(target_os = "windows")]
    {
        const RESOURCE_FILE: &str = "resources/windows/gcabb.rc";
        println!("cargo:rerun-if-changed={RESOURCE_FILE}");
        println!("cargo:rerun-if-changed=resources/windows/gcabb.ico");
        embed_resource::compile(RESOURCE_FILE, embed_resource::NONE)
            .manifest_optional()
            .expect("failed to embed GCABB Windows icon");
    }
}
