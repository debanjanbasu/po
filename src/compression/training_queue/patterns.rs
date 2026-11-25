//! Pattern types, extraction, and aggregation for dictionary training.

use std::{
    collections::{HashMap, HashSet},
    mem,
};

use super::AggregatedPatterns;
use crate::config;

const MAX_WORD_FREQ_ENTRIES: usize = 5000;
const MAX_EMOJI_FREQ_ENTRIES: usize = 1000;
const MAX_TEMPLATE_ENTRIES: usize = 500;
/// Extracted patterns from a chat message.
#[derive(Debug, Clone, Default)]
pub struct ChatPattern {
    /// Word and n-gram frequencies (key -> count).
    pub word_freq: HashMap<String, u64>,
    /// Emoji frequencies (emoji -> count).
    pub emoji_freq: HashMap<String, u64>,
    /// Structural templates (e.g., "sender:*").
    pub templates: Vec<String>,
}

/// Extracted patterns from structured data.
#[derive(Debug, Clone, Default)]
pub struct StructuredPattern {
    /// Template with values replaced by wildcards.
    pub template: String,
    /// Field keys that appear in the record.
    pub keys: Vec<String>,
}

/// Extract chat patterns from a message.
///
/// # Privacy
/// - Extracts n-grams (1-3 words) for compression optimization
/// - Collects emoji frequencies
/// - Never stores raw message content
pub fn extract_chat_pattern(sender: &str, content: &str) -> ChatPattern {
    let mut pattern = ChatPattern::default();

    // Extract word n-grams
    let words: Vec<&str> = content
        .split_whitespace()
        .filter(|w| w.len() >= 2 && w.len() <= 50) // Filter noise
        .collect();

    for n in 1..=config::compression::MAX_NGRAM_LEN.min(words.len()) {
        for window in words.windows(n) {
            let ngram = window.join(" ");
            *pattern.word_freq.entry(ngram).or_default() += 1;
        }
    }

    // Extract emojis
    for emoji in extract_emojis(content) {
        *pattern.emoji_freq.entry(emoji.to_string()).or_default() += 1;
    }

    // Structural template (sender pattern)
    pattern.templates.push(format!("{sender}:*"));

    pattern
}

/// Extract emojis from text.
///
/// Handles common emoji patterns including:
/// - Unicode emojis (😀, 🎉, etc.)
/// - Emoji sequences with skin tone modifiers
/// - ZWJ sequences (family emojis, etc.)
fn extract_emojis(text: &str) -> Vec<&str> {
    let mut emojis = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i].1;
        // Check for emoji ranges
        if is_emoji_char(c) {
            // Find the end of this emoji sequence
            let start = i;
            i += 1;

            // Consume variation selectors, skin tone modifiers, ZWJ sequences
            while i < chars.len() {
                let next = chars[i].1;
                if is_emoji_modifier(next) || next == '\u{200D}' {
                    // ZWJ - join with next emoji
                    i += 1;
                    // After ZWJ, skip up to 2 non-emoji modifiers (e.g. variation
                    // selectors) but only if followed by an actual emoji char.
                    let mut skip_count = 0;
                    while i < chars.len()
                        && !is_emoji_char(chars[i].1)
                        && !is_emoji_modifier(chars[i].1)
                        && skip_count < 2
                    {
                        i += 1;
                        skip_count += 1;
                    }
                    // If we didn't land on an emoji, the ZWJ sequence is broken;
                    // rewind the skipped non-emoji chars so they aren't included.
                    if i < chars.len()
                        && !is_emoji_char(chars[i].1)
                        && !is_emoji_modifier(chars[i].1)
                    {
                        i -= skip_count;
                        break;
                    }
                    if i >= chars.len() {
                        i -= skip_count;
                        break;
                    }
                } else if is_emoji_char(next) {
                    // Might be part of a sequence
                    i += 1;
                } else {
                    break;
                }
            }

            // Extract the emoji slice using char byte offsets
            let end_byte = chars.get(i).map_or(text.len(), |&(b, _)| b);
            if let Some(emoji) = text.get(chars[start].0..end_byte) {
                emojis.push(emoji);
            }
        } else {
            i += 1;
        }
    }

    emojis
}

/// Check if a character is an emoji.
const fn is_emoji_char(c: char) -> bool {
    matches!(
        c,
        '\u{1F1E0}'..='\u{1F1FF}' // Flags
        | '\u{1F600}'..='\u{1F64F}' // Emoticons
        | '\u{1F680}'..='\u{1F6FF}' // Transport
        | '\u{1F900}'..='\u{1F9FF}' // Supplemental Symbols
        | '\u{1FA00}'..='\u{1FA6F}' // Chess Symbols
        | '\u{1FA70}'..='\u{1FAFF}' // Symbols and Pictographs Extended-A
        | '\u{1F300}'..='\u{1F5FF}' // Misc Symbols and Pictographs
        | '\u{2600}'..='\u{26FF}' // Misc symbols
        | '\u{2700}'..='\u{27BF}' // Dingbats
        | '\u{2300}'..='\u{23FF}' // Misc Technical
        | '\u{2B50}' // Star
        | '\u{2B55}' // Circle
        | '\u{3030}' // Wavy dash
        | '\u{303D}' // Part alternation
        | '\u{3297}' // Circled ideograph
        | '\u{3299}' // Circled ideograph
        | '\u{00A9}' // Copyright
        | '\u{00AE}' // Registered
        | '\u{2122}' // TM
    )
}

/// Check if a character is an emoji modifier (skin tone, etc.).
const fn is_emoji_modifier(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}') // Skin tone modifiers
        || matches!(c, '\u{FE00}'..='\u{FE0F}') // Variation selectors
}

/// Extract a structured pattern from field names.
///
/// # Privacy
/// - Records only field names (keys), never values
/// - Generates a structural template from the sorted key set
pub fn extract_structured_pattern(keys: &[&str]) -> StructuredPattern {
    let mut sorted_keys: Vec<String> = keys.iter().map(ToString::to_string).collect();
    sorted_keys.sort();
    StructuredPattern {
        template: sorted_keys.join(":"),
        keys: sorted_keys,
    }
}

/// Merge a chat pattern into aggregated patterns.
pub(super) fn merge_chat_pattern(
    aggregated: &mut AggregatedPatterns,
    pattern: &ChatPattern,
    existing_templates: &mut HashSet<String>,
) {
    for (word, count) in &pattern.word_freq {
        if let Some(total) = aggregated.word_freq.get_mut(word.as_str()) {
            *total += count;
        } else {
            aggregated.word_freq.insert(word.clone(), *count);
        }
    }
    for (emoji, count) in &pattern.emoji_freq {
        if let Some(total) = aggregated.emoji_freq.get_mut(emoji.as_str()) {
            *total += count;
        } else {
            aggregated.emoji_freq.insert(emoji.clone(), *count);
        }
    }
    for template in &pattern.templates {
        if !existing_templates.contains(template.as_str()) {
            existing_templates.insert(template.clone());
            aggregated.templates.push(template.clone());
        }
    }
}

/// Merge a structured pattern into aggregated patterns.
pub(super) fn merge_structured_pattern(
    aggregated: &mut AggregatedPatterns,
    pattern: &StructuredPattern,
) {
    let template_key = format!("template:{}", pattern.template);
    if let Some(total) = aggregated.word_freq.get_mut(template_key.as_str()) {
        *total += 1;
    } else {
        aggregated.word_freq.insert(template_key, 1);
    }
    for key in &pattern.keys {
        let key_entry = format!("key:{key}");
        if let Some(total) = aggregated.word_freq.get_mut(key_entry.as_str()) {
            *total += 1;
        } else {
            aggregated.word_freq.insert(key_entry, 1);
        }
    }
}

/// Drops the least-frequent entries past `max_entries` so a frequency table
/// cannot outgrow the training bound it feeds.
pub fn cap_freq_map(map: &mut HashMap<String, u64>, max_entries: usize) {
    if map.len() <= max_entries {
        return;
    }
    let mut entries: Vec<(String, u64)> = mem::take(map).into_iter().collect();
    entries.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.truncate(max_entries);
    *map = entries.into_iter().collect();
}

impl AggregatedPatterns {
    /// Caps persisted frequency tables and templates to their training bounds.
    pub fn cap_entries(&mut self) {
        cap_freq_map(&mut self.word_freq, MAX_WORD_FREQ_ENTRIES);
        cap_freq_map(&mut self.emoji_freq, MAX_EMOJI_FREQ_ENTRIES);
        self.templates.truncate(MAX_TEMPLATE_ENTRIES);
    }
}

fn capped_repetitions(count: u64, cap: usize) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX).min(cap)
}

fn estimate_sample_capacity(
    aggregated: &AggregatedPatterns,
    weighted_words: &[(&String, &u64)],
    weighted_emojis: &[(&String, &u64)],
) -> usize {
    let word_slots: usize = weighted_words
        .iter()
        .take(MAX_WORD_FREQ_ENTRIES)
        .map(|(_, count)| capped_repetitions(**count, 10))
        .sum();
    let emoji_slots: usize = weighted_emojis
        .iter()
        .take(MAX_EMOJI_FREQ_ENTRIES)
        .map(|(_, count)| capped_repetitions(**count, 5))
        .sum();
    word_slots
        .saturating_add(emoji_slots)
        .saturating_add(aggregated.templates.len().min(MAX_TEMPLATE_ENTRIES))
}

/// Convert aggregated patterns to training samples for zstd.
pub fn patterns_to_training_samples(aggregated: &AggregatedPatterns) -> Vec<&[u8]> {
    let mut weighted_words: Vec<_> = aggregated.word_freq.iter().collect();
    weighted_words.sort_unstable_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

    let mut weighted_emojis: Vec<_> = aggregated.emoji_freq.iter().collect();
    weighted_emojis.sort_unstable_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

    let mut templates: Vec<_> = aggregated.templates.iter().collect();
    templates.sort_unstable();

    let mut samples = Vec::with_capacity(estimate_sample_capacity(
        aggregated,
        &weighted_words,
        &weighted_emojis,
    ));

    for (word, count) in weighted_words.iter().take(MAX_WORD_FREQ_ENTRIES) {
        for _ in 0..capped_repetitions(**count, 10) {
            samples.push(word.as_bytes());
        }
    }

    for (emoji, count) in weighted_emojis.iter().take(MAX_EMOJI_FREQ_ENTRIES) {
        for _ in 0..capped_repetitions(**count, 5) {
            samples.push(emoji.as_bytes());
        }
    }

    for template in templates.iter().take(MAX_TEMPLATE_ENTRIES) {
        samples.push(template.as_bytes());
    }

    samples
}
