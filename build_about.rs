//! Build-time codegen: reads `shared/about.json` and emits `about_generated.rs`
//! into `OUT_DIR` so the app can `include!` it without runtime JSON parsing.

use anyhow::{Context, Result};
use std::{fmt::Write, fs, path::Path};

pub fn generate(out_dir: &Path) -> Result<()> {
    let json_str =
        fs::read_to_string("shared/about.json").context("failed to read shared/about.json")?;

    let json: serde_json::Value =
        serde_json::from_str(&json_str).context("failed to parse shared/about.json")?;

    let tagline = json["tagline"].as_str().unwrap_or("");
    let veilid_url = json["links"]["veilid"].as_str().unwrap_or("");
    let automerge_url = json["links"]["automerge"].as_str().unwrap_or("");
    let dioxus_url = json["links"]["dioxus"].as_str().unwrap_or("");
    let zstd_url = json["links"]["zstd"].as_str().unwrap_or("");
    let capnproto_url = json["links"]["capnproto"].as_str().unwrap_or("");

    let mut code = String::from(
        "// Auto-generated from `shared/about.json` — do not edit.\n\n\
         pub struct AboutFeature {\n    \
             pub icon: &'static str,\n    \
             pub short: &'static str,\n\
         }\n\n",
    );

    writeln!(code, "pub const TAGLINE: &str = {tagline:?};")?;
    writeln!(
        code,
        "pub const LINKS: &[(&str, &str)] = &[\n\
         \x20   ({veilid_url:?}, \"Veilid\"),\n\
         \x20   ({automerge_url:?}, \"Automerge\"),\n\
         \x20   ({dioxus_url:?}, \"Dioxus\"),\n\
         \x20   ({zstd_url:?}, \"Zstd\"),\n\
         \x20   ({capnproto_url:?}, \"Cap'n Proto\"),\n\
         ];\n"
    )?;

    code.push_str("pub const FEATURES: &[AboutFeature] = &[\n");
    if let Some(features) = json["features"].as_array() {
        for f in features {
            let icon = f["icon"].as_str().unwrap_or("");
            let short = f["short"].as_str().unwrap_or("");
            writeln!(
                code,
                "    AboutFeature {{ icon: {icon:?}, short: {short:?} }},",
            )?;
        }
    }
    code.push_str("];\n");

    fs::write(out_dir.join("about_generated.rs"), code)
        .context("failed to write about_generated.rs")?;

    Ok(())
}
