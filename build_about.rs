//! Build-time codegen: reads `shared/about.json` and emits `about_generated.rs`
//! into `OUT_DIR` so the app can `include!` it without runtime JSON parsing.

use anyhow::{Context, Result};
use std::{fmt::Write, fs, path::Path};

/// Typed shape of `shared/about.json`. A missing or wrong-typed field fails
/// the build with a serde error naming it, instead of silently generating
/// empty string constants.
#[derive(serde::Deserialize)]
struct AboutJson {
    tagline: String,
    #[serde(rename = "tech_badges")]
    #[allow(dead_code)] // validated on parse; landing owns the display copy
    tech_badges: Vec<String>,
    features: Vec<AboutFeatureJson>,
    links: AboutLinks,
}

#[derive(serde::Deserialize)]
struct AboutFeatureJson {
    icon: String,
    #[allow(dead_code)]
    title: String,
    short: String,
    #[allow(dead_code)]
    detail: String,
}

#[derive(serde::Deserialize)]
struct AboutLinks {
    veilid: String,
    automerge: String,
    dioxus: String,
    zstd: String,
    capnproto: String,
}

pub fn generate(out_dir: &Path) -> Result<()> {
    let json_str =
        fs::read_to_string("shared/about.json").context("failed to read shared/about.json")?;

    let json: AboutJson =
        serde_json::from_str(&json_str).context("failed to parse shared/about.json")?;

    let mut code = String::from(
        "// Auto-generated from `shared/about.json` — do not edit.\n\n\
         pub struct AboutFeature {\n    \
             pub icon: &'static str,\n    \
             pub short: &'static str,\n\
         }\n\n",
    );

    writeln!(code, "pub const TAGLINE: &str = {:?};", json.tagline)?;
    writeln!(
        code,
        "pub const LINKS: &[(&str, &str)] = &[\n\
         \x20   ({:?}, \"Veilid\"),\n\
         \x20   ({:?}, \"Automerge\"),\n\
         \x20   ({:?}, \"Dioxus\"),\n\
         \x20   ({:?}, \"Zstd\"),\n\
         \x20   ({:?}, \"Cap'n Proto\"),\n\
         ];\n",
        json.links.veilid,
        json.links.automerge,
        json.links.dioxus,
        json.links.zstd,
        json.links.capnproto
    )?;

    code.push_str("pub const FEATURES: &[AboutFeature] = &[\n");
    for f in &json.features {
        writeln!(
            code,
            "    AboutFeature {{ icon: {:?}, short: {:?} }},",
            f.icon, f.short
        )?;
    }
    code.push_str("];\n");

    fs::write(out_dir.join("about_generated.rs"), code)
        .context("failed to write about_generated.rs")?;

    Ok(())
}
