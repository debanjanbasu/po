//! Build script that:
//! 1. Compiles Cap'n Proto (`.capnp`) schemas via `recapnc`
//! 2. Trains a zstd dictionary for structured data compression
//! 3. Generates `about_generated.rs` from `shared/about.json`
//!
//! The dictionary is cached via a hash file — training only runs when samples
//! or training parameters change. On warm builds the step is skipped entirely.
//!
//! Training samples come from:
//! - `assets/dict/samples/structured.txt` — key-value records (null-separated)
//! - `assets/dict/samples/structured_capnp.bin` — serialized structured data (optional)

mod build_about;

use anyhow::{Context, Result, ensure};
use std::{
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

/// Build-time dictionary parameters are part of the cache key; bump the trainer
/// version whenever training behavior changes.
const DICT_SIZE: usize = 16_384;
const TRAINER_VERSION: u32 = 1;

/// Deterministic hash of training parameters and samples, for the cache key.
/// `DefaultHasher` (`SipHash` with fixed keys) is deterministic across runs and
/// processes; the hash only has to match the app's own `.zdict.hash` file.
fn hash_samples(samples: &[Vec<u8>]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    DICT_SIZE.hash(&mut hasher);
    TRAINER_VERSION.hash(&mut hasher);
    for sample in samples {
        sample.len().hash(&mut hasher);
        hasher.write(sample);
    }
    hasher.finish()
}

/// Check if cached dict and hash file match the current training cache key.
/// Returns `true` when the cached dictionary is still valid.
fn is_cached(dict_path: &Path, expected_hash: u64) -> bool {
    if !dict_path.exists() {
        return false;
    }
    let hash_path = dict_path.with_extension("zdict.hash");
    let Ok(stored) = fs::read_to_string(&hash_path) else {
        return false;
    };
    stored.trim() == expected_hash.to_string()
}

/// Train a zstd dictionary from samples, writing both the dict and hash file.
/// Skips entirely when no usable samples remain after filtering, or when the
/// cached dict already matches the current training cache key.
fn train_dict(name: &str, samples: &[Vec<u8>], dict_dir: &Path) -> Result<()> {
    let dict_path = dict_dir.join(format!("{name}.zdict"));
    let hash_path = dict_path.with_extension("zdict.hash");

    if samples.is_empty() {
        println!(
            "cargo:warning=Skipped {name} dictionary training: no usable samples remained after filtering blank/whitespace/comment-only entries; keeping {}",
            dict_path.display()
        );
        return Ok(());
    }

    let hash = hash_samples(samples);
    if is_cached(&dict_path, hash) {
        return Ok(());
    }

    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    let dict = zstd::dict::from_samples(&refs, DICT_SIZE)
        .with_context(|| format!("failed to train {name} dictionary"))?;

    fs::write(&dict_path, &dict).with_context(|| format!("failed to write {name}.zdict"))?;
    fs::write(&hash_path, hash.to_string())
        .with_context(|| format!("failed to write {name}.zdict.hash"))?;

    println!(
        "cargo:warning=Trained {name} dictionary: {} bytes from {} samples",
        dict.len(),
        samples.len()
    );

    Ok(())
}

// ── Cap'n Proto compilation ────────────────────────────────────────────────

fn compile_schemas() -> Result<()> {
    const REQUIRED_SCHEMAS: [&str; 8] = [
        "app_state.capnp",
        "chat.capnp",
        "group_chat.capnp",
        "learned_samples.capnp",
        "local_conversation.capnp",
        "pending_operation.capnp",
        "totp.capnp",
        "user.capnp",
    ];
    let schema_dir = PathBuf::from("src/schemas");
    ensure!(
        schema_dir.is_dir(),
        "required schema directory src/schemas is missing"
    );

    let mut cmd = recapnc::CapnpCommand::new();
    cmd.src_prefix("src/schemas");
    for name in REQUIRED_SCHEMAS {
        let path = schema_dir.join(name);
        let metadata = fs::metadata(&path).with_context(|| {
            format!("required Cap'n Proto schema {} is missing", path.display())
        })?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "Cap'n Proto schema {} is empty",
            path.display()
        );
        cmd.file(&path);
    }
    cmd.write_to_out_dir();

    Ok(())
}

// ── Training sample loading ────────────────────────────────────────────────

fn usable_text_sample(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    (!trimmed.is_empty() && !trimmed.starts_with('#')).then_some(trimmed)
}

/// Load training samples from an external text file.
/// Format:
/// - One sample per line
/// - Whitespace-only lines are ignored
/// - Lines whose trimmed content starts with `#` are comments (ignored)
/// - Remaining lines are trimmed before conversion to bytes
fn load_samples(path: &Path) -> Result<Vec<Vec<u8>>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    let samples: Vec<Vec<u8>> = content
        .lines()
        .filter_map(usable_text_sample)
        .map(|line| line.as_bytes().to_vec())
        .collect();

    Ok(samples)
}

/// Load length-prefixed binary samples from a file.
/// Format: [length: 4 bytes][data] repeated
fn load_binary_samples(path: &Path) -> Result<Vec<Vec<u8>>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;

    let mut samples = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        ensure!(
            offset + 4 <= data.len(),
            "trailing bytes in {}",
            path.display()
        );
        let len = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .context("invalid sample length in binary samples file")?,
        ) as usize;
        offset += 4;

        ensure!(
            offset + len <= data.len(),
            "truncated sample in {}",
            path.display()
        );
        if len == 0 {
            continue;
        }

        samples.push(data[offset..offset + len].to_vec());
        offset += len;
    }

    Ok(samples)
}

// ── Entry point ────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=src/schemas");
    println!("cargo:rerun-if-changed=assets/dict/samples");
    println!("cargo:rerun-if-changed=assets/dict/structured.zdict");
    println!("cargo:rerun-if-changed=assets/dict/structured.zdict.hash");
    println!("cargo:rerun-if-changed=shared/about.json");

    let out_dir = PathBuf::from(env::var("OUT_DIR").context("OUT_DIR not set")?);

    // 1. Compile Cap'n Proto schemas
    compile_schemas()?;

    // 2. Load training samples and train unified zstd dictionary
    let samples_dir = PathBuf::from("assets/dict/samples");
    let mut samples = load_samples(&samples_dir.join("structured.txt"))?;
    samples.extend(load_binary_samples(
        &samples_dir.join("structured_capnp.bin"),
    )?);
    let dict_dir = PathBuf::from("assets/dict");
    fs::create_dir_all(&dict_dir)?;
    train_dict("structured", &samples, &dict_dir)?;

    // 3. Generate about_generated.rs from shared/about.json
    build_about::generate(&out_dir)?;

    Ok(())
}
