//! Binary to generate serialized training samples for dictionary compression.
//!
//! This binary generates actual Cap'n Proto and Automerge CRDT serialized data
//! that matches the runtime serialization formats used by the application.
//!
//! Output:
//! - `assets/dict/samples/structured_capnp.bin` - Cap'n Proto serialized data (chat + structured)

use std::{fmt::Write, fs, path::Path};

use anyhow::Result;

// The generated Cap'n Proto modules self-allow `unused`/`unsafe_code`; the
// pedantic lints that still fire on their output (module_inception,
// elidable_lifetime_names, unreadable_literal, use_self) are allow-listed
// exactly — generated code is not hand-fixable here.
#[allow(
    clippy::module_inception,
    clippy::elidable_lifetime_names,
    clippy::unreadable_literal,
    clippy::use_self
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/mod.rs"));
}

/// Reuses the runtime packer verbatim (`src/helpers/capnp.rs`) instead
/// of a byte-copied twin: the samples train the dictionary used against
/// runtime encoding, so they must share one code path.
#[path = "../helpers/capnp.rs"]
mod capnp;
use capnp::{capnp_enum_convert, unpack};
use generated::chat_capnp;

enum SampleStatus {
    Pending,
    Active,
}
capnp_enum_convert!(SampleStatus => chat_capnp::ContactStatus {
    Pending,
    Active,
});
use generated::{group_chat_capnp, totp_capnp, user_capnp};

const MESSAGE_PAGE_SCHEMA_VERSION: u32 = 1;
const TOTP_SCHEMA_VERSION: u32 = 1;
const USER_SCHEMA_VERSION: u32 = 1;
const USER_CRYPTO_VERSION: u32 = 1;
const USER_NONCE_LEN: usize = 24;

fn main() -> Result<()> {
    let out_dir = Path::new("assets/dict/samples");
    fs::create_dir_all(out_dir)?;

    let _ = (
        SampleStatus::from(chat_capnp::ContactStatus::from(SampleStatus::Pending)),
        SampleStatus::from(chat_capnp::ContactStatus::from(SampleStatus::Active)),
        unpack::<user_capnp::User, ()>(&[], |_| Ok(())),
    );
    generate_structured_samples(out_dir)?;
    println!(
        "Generated serialized training samples in {}",
        out_dir.display()
    );
    Ok(())
}

fn generate_structured_samples(out_dir: &Path) -> Result<()> {
    let mut capnp_samples: Vec<Vec<u8>> = Vec::new();

    let issuers = [
        "Google",
        "GitHub",
        "Amazon",
        "Microsoft",
        "Dropbox",
        "Netflix",
    ];
    let accounts = ["alice@example.com", "bob@test.com", "user@domain.org"];
    let secrets = [
        "JBSWY3DPEHPK3PXP",
        "GEZDGNBVGY3TQOJQ",
        "HXDMVJRCJJYRBNDJ",
        "KVKFKRCPNZQUYMBX",
    ];
    let algorithms = ["SHA1", "SHA256", "SHA512"];
    let digit_counts = [6u32, 8];
    let periods = [30u32, 60];

    for issuer in &issuers {
        for account in &accounts {
            for (secret_idx, secret) in secrets.iter().enumerate() {
                let algorithm = algorithms[secret_idx % algorithms.len()];
                let digits = digit_counts[secret_idx % digit_counts.len()];
                let period = periods[secret_idx % periods.len()];

                let capnp_bytes =
                    serialize_totp_token(issuer, account, secret, algorithm, digits, period)?;
                capnp_samples.push(capnp_bytes);
            }
        }
    }

    let usernames = ["alice", "bob", "charlie", "diana"];
    let dht_keys = [
        "VLD0:abc123:def456:ghi789",
        "VLD0:jkl012:mno345:pqr678",
        "VLD0:stu901:vwx234:yza567",
        "VLD0:bcd890:efg123:hij456",
    ];

    for (i, username) in usernames.iter().enumerate() {
        let dob = i32::try_from(i)
            .unwrap_or(i32::MAX)
            .saturating_add(19_900_101);
        let capnp_bytes = serialize_user_profile(username, dht_keys[i % dht_keys.len()], dob)?;
        capnp_samples.push(capnp_bytes);
    }

    // Chat message samples (merged into unified structured output)
    let chat_usernames = ["alice", "bob", "charlie", "diana", "po"];
    let messages = [
        "Hey! How's it going?",
        "Pretty good! Just finished lunch",
        "Nice! What did you have?",
        "Homemade pizza, turned out great!",
        "Did you see the meeting invite?",
        "Yeah, 3pm right?",
        "Yep! Conference room B",
        "Got it, thanks!",
        "Also, did you review my PR?",
        "Not yet, I'll check it after the meeting",
        "Movie night still on?",
        "Yes! 7pm at my place",
        "Should I bring snacks?",
        "That would be awesome!",
        "I'll grab some chips and drinks",
        "Perfect! See you then",
    ];

    for batch_start in (0..messages.len()).step_by(5) {
        let batch_end = (batch_start + 5).min(messages.len());
        let batch_messages: Vec<_> = messages[batch_start..batch_end]
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                let sender_idx = (batch_start + i) % chat_usernames.len();
                (chat_usernames[sender_idx], *msg)
            })
            .collect();
        capnp_samples.push(serialize_message_page(
            &batch_messages,
            batch_start as u64 + 1,
        )?);
    }

    let capnp_path = out_dir.join("structured_capnp.bin");
    write_length_prefixed(&capnp_path, &capnp_samples)?;

    println!("Generated {} Cap'n Proto samples", capnp_samples.len());
    Ok(())
}

/// Serializes a `group_chat.capnp` `MessagePage` — the same shape the runtime
/// stores for conversation messages — so dictionary training sees real bytes.
fn serialize_message_page(messages: &[(&str, &str)], start_seq: u64) -> Result<Vec<u8>> {
    // Mirrors the runtime `generate_msg_id` format: 16 hex chars of epoch
    // millis followed by 16 hex chars of a monotonic sample counter.
    const SAMPLE_EPOCH_MILLIS: u64 = 1_755_000_000_000;

    capnp::pack::<group_chat_capnp::MessagePage>(|mut root| {
        root.schema_version().set(MESSAGE_PAGE_SCHEMA_VERSION);
        root.page_seq_start().set(start_seq);
        root.page_seq_end()
            .set(start_seq + messages.len().saturating_sub(1) as u64);
        let len = capnp::capnp_u32(messages.len());
        let mut list = root.messages().init(len);
        for (i, (sender, content)) in messages.iter().enumerate() {
            let idx = capnp::capnp_u32(i);
            let counter = start_seq + i as u64;
            let timestamp = SAMPLE_EPOCH_MILLIS + counter * 1_000;
            let mut message = list.at(idx).get();
            message.seq().set(counter);
            message.sender_username().set_str(sender);
            message.content().set_str(content);
            message.timestamp().set(timestamp);
            message
                .msg_id()
                .set_str(&format!("{timestamp:016x}{counter:016x}"));
            message.status().set(group_chat_capnp::MessageStatus::Sent);
        }
    })
}

fn serialize_totp_token(
    issuer: &str,
    account: &str,
    secret: &str,
    algorithm: &str,
    digits: u32,
    period: u32,
) -> Result<Vec<u8>> {
    capnp::pack::<totp_capnp::TotpTokenList>(|mut root| {
        root.schema_version().set(TOTP_SCHEMA_VERSION);
        root.last_updated().set(0);
        let mut tokens = root.tokens().init(1);
        let mut token = tokens.at(0).get();
        token.issuer().set_str(issuer);
        token.account_name().set_str(account);
        token.secret().set_str(secret);
        token.algorithm().set_str(algorithm);
        token.digits().set(digits);
        token.period().set(period);
        token.created_at().set(0);
        token.updated_at().set(0);
    })
}

fn sample_account_id(username: &str) -> String {
    let mut raw = [0u8; 16];
    for (i, b) in username.as_bytes().iter().enumerate() {
        let idx = i % raw.len();
        let step = i.to_le_bytes()[0].wrapping_mul(17);
        raw[idx] = raw[idx].wrapping_add(*b).wrapping_add(step);
    }
    let mut out = String::with_capacity(32);
    for b in raw {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn sample_user_nonce(username: &str) -> [u8; USER_NONCE_LEN] {
    let mut nonce = [0u8; USER_NONCE_LEN];
    for (i, b) in username.as_bytes().iter().enumerate() {
        let idx = i % USER_NONCE_LEN;
        nonce[idx] = nonce[idx].wrapping_add(*b).wrapping_add(i.to_le_bytes()[0]);
    }
    nonce
}

fn serialize_user_profile(username: &str, dht_key: &str, dob: i32) -> Result<Vec<u8>> {
    let nonce = sample_user_nonce(username);
    let ciphertext = format!("encrypted-keypair:{username}");

    capnp::pack::<user_capnp::User>(|mut root| {
        root.schema_version().set(USER_SCHEMA_VERSION);
        root.username().set_str(username);
        let mut key_pair = root.key_pair().init();
        key_pair.ciphertext().set_slice(ciphertext.as_bytes());
        key_pair.nonce().set_slice(&nonce);
        root.dht_key().set_str(dht_key);
        root.dob().set(dob);
        root.pin_hash().set_str("");
        root.terms_accepted().set(true);
        root.biometrics_enabled().set(false);
        root.account_id().set_str(&sample_account_id(username));
        root.crypto_version().set(USER_CRYPTO_VERSION);
    })
}

fn write_length_prefixed(path: &Path, samples: &[Vec<u8>]) -> Result<()> {
    let mut buf = Vec::new();
    for sample in samples {
        let len = capnp::capnp_u32(sample.len());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(sample);
    }
    fs::write(path, &buf)?;
    Ok(())
}
