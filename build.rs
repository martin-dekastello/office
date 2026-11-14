fn main() {
    // `include_dir!` in src/plugin.rs embeds skills/. Cargo checks a directory
    // given here recursively, so adding a skill rebuilds as editing one does.
    println!("cargo:rerun-if-changed=skills");
}
