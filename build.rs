// Build script — no code generation.
//
// Unicode data tables are pre-generated and committed to src/ by
// scripts/gen_unicode_tables.  To regenerate after a Unicode update:
//
//   sh scripts/fetch_unicode_data.sh [VERSION]
//   cargo run --manifest-path scripts/gen_unicode_tables/Cargo.toml

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
