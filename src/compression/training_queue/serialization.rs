//! Cap'n Proto serialization for training queue models.

use std::collections::HashMap;

use anyhow::Result;
use recapn::{
    prelude::generated::{AccessorMut, List, ListReader, Struct},
    rpc::Empty,
};

use crate::{
    helpers::capnp::{capnp_u32, pack, unpack},
    models::generated::learned_samples_capnp::{self, FreqEntry},
};

use super::{
    AggregatedPatterns, CHAT_PATTERNS_SCHEMA_VERSION, STRUCTURED_PATTERNS_SCHEMA_VERSION,
    TRAINING_METADATA_SCHEMA_VERSION, TrainingMetadata,
};

type FreqEntryListReader<'a> = List<Struct<FreqEntry>, ListReader<'a, Empty>>;

pub(super) fn sorted_freq_entries(map: &HashMap<String, u64>) -> Vec<(&str, u64)> {
    let mut entries: Vec<_> = map.iter().map(|(k, &v)| (k.as_str(), v)).collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    entries
}

fn write_freq_entries(
    field: AccessorMut<'_, '_, Empty, List<Struct<FreqEntry>>>,
    entries: &[(&str, u64)],
) {
    let len = capnp_u32(entries.len());
    let mut list = field.init(len);
    for (index, (key, count)) in entries.iter().enumerate() {
        let mut entry = list.at(capnp_u32(index)).get();
        entry.key().set_str(key);
        entry.count().set(*count);
    }
}

fn read_freq_entries_into(
    reader: &FreqEntryListReader<'_>,
    prefix: &str,
    out: &mut HashMap<String, u64>,
) -> Result<()> {
    for i in 0..reader.len() {
        let entry = reader.at(i);
        let key = entry.key().get().as_str()?;
        let full_key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}{key}")
        };
        out.insert(full_key, entry.count());
    }
    Ok(())
}

pub(super) fn serialize_chat_patterns(patterns: &AggregatedPatterns) -> Result<Vec<u8>> {
    let word_entries = sorted_freq_entries(&patterns.word_freq);
    let emoji_entries = sorted_freq_entries(&patterns.emoji_freq);

    pack::<learned_samples_capnp::ChatPatterns>(|mut root| {
        root.schema_version().set(CHAT_PATTERNS_SCHEMA_VERSION);
        root.last_updated().set(patterns.last_updated);

        write_freq_entries(root.word_freq(), &word_entries);

        let templates_len = capnp_u32(patterns.templates.len());
        let mut templates = root.templates().init(templates_len);
        for (index, template) in patterns.templates.iter().enumerate() {
            templates.at(capnp_u32(index)).set_str(template);
        }

        write_freq_entries(root.emoji_freq(), &emoji_entries);

        root.type_freq().init(0); // Reserved field — always empty; see typeFreq in learned_samples.capnp.
    })
}

pub(super) fn deserialize_chat_patterns(bytes: &[u8]) -> Result<(AggregatedPatterns, u32)> {
    unpack::<learned_samples_capnp::ChatPatterns, _>(bytes, |root| {
        let word_reader = root.word_freq().get();
        let mut word_freq = HashMap::with_capacity(word_reader.len() as usize);
        read_freq_entries_into(&word_reader, "", &mut word_freq)?;

        let templates_reader = root.templates().get();
        let templates = (0..templates_reader.len())
            .map(|i| Ok(templates_reader.at(i).get().as_str()?.to_string()))
            .collect::<Result<Vec<_>>>()?;

        let emoji_reader = root.emoji_freq().get();
        let mut emoji_freq = HashMap::with_capacity(emoji_reader.len() as usize);
        read_freq_entries_into(&emoji_reader, "", &mut emoji_freq)?;

        Ok((
            AggregatedPatterns {
                word_freq,
                emoji_freq,
                templates,
                last_updated: root.last_updated(),
            },
            root.schema_version(),
        ))
    })
}

fn write_prefixed_freq_entries(
    field: AccessorMut<'_, '_, Empty, List<Struct<FreqEntry>>>,
    words: &HashMap<String, u64>,
    prefix: &str,
    include_unprefixed: bool,
) {
    let mut entries: Vec<(&str, u64)> = words
        .iter()
        .filter_map(|(entry, &count)| {
            entry.strip_prefix(prefix).map_or_else(
                || {
                    (include_unprefixed && !entry.starts_with("key:"))
                        .then_some((entry.as_str(), count))
                },
                |stripped| Some((stripped, count)),
            )
        })
        .collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    write_freq_entries(field, &entries);
}

pub(super) fn serialize_structured_patterns(patterns: &AggregatedPatterns) -> Result<Vec<u8>> {
    pack::<learned_samples_capnp::StructuredPatterns>(|mut root| {
        root.schema_version()
            .set(STRUCTURED_PATTERNS_SCHEMA_VERSION);
        root.last_updated().set(patterns.last_updated);

        write_prefixed_freq_entries(root.template_freq(), &patterns.word_freq, "template:", true);
        write_prefixed_freq_entries(root.key_freq(), &patterns.word_freq, "key:", false);
    })
}

pub(super) fn deserialize_structured_patterns(bytes: &[u8]) -> Result<(AggregatedPatterns, u32)> {
    unpack::<learned_samples_capnp::StructuredPatterns, _>(bytes, |root| {
        let templates = root.template_freq().get();
        let keys = root.key_freq().get();
        let mut word_freq = HashMap::with_capacity((templates.len() + keys.len()) as usize);

        read_freq_entries_into(&templates, "template:", &mut word_freq)?;
        read_freq_entries_into(&keys, "key:", &mut word_freq)?;

        Ok((
            AggregatedPatterns {
                word_freq,
                emoji_freq: HashMap::new(),
                templates: Vec::new(),
                last_updated: root.last_updated(),
            },
            root.schema_version(),
        ))
    })
}

pub(super) fn serialize_metadata(metadata: &TrainingMetadata) -> Result<Vec<u8>> {
    pack::<learned_samples_capnp::TrainingMetadata>(|mut root| {
        root.schema_version().set(TRAINING_METADATA_SCHEMA_VERSION);
        root.last_training().set(metadata.last_training);
    })
}

pub(super) fn deserialize_metadata(bytes: &[u8]) -> Result<(TrainingMetadata, u32)> {
    unpack::<learned_samples_capnp::TrainingMetadata, _>(bytes, |root| {
        Ok((
            TrainingMetadata {
                last_training: root.last_training(),
            },
            root.schema_version(),
        ))
    })
}
