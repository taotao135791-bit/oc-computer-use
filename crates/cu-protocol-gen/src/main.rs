//! Writes `protocol/computer-use.schema.json` from the Rust wire types.
//!
//! Invoked by `pnpm generate:protocol` (which then regenerates the TypeScript
//! bindings from the schema). `pnpm check:protocol` regenerates both and
//! fails on any drift.

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let schema = cu_protocol_gen::build_protocol_schema();
    let rendered = serde_json::to_string_pretty(&schema)?;
    if std::env::var_os("CU_PROTOCOL_GEN_STDOUT").is_some() {
        // Drift-check mode: print the document instead of writing it, so the
        // check script can compare it against the committed file.
        println!("{rendered}");
        return Ok(());
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<crate> is two levels below the repo root");
    let out = repo_root.join("protocol").join("computer-use.schema.json");
    std::fs::write(&out, format!("{rendered}\n"))?;
    eprintln!("wrote {}", out.display());
    Ok(())
}
