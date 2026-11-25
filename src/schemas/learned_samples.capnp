@0xdf1c2b3a4e5f6789;

# Learned dictionary samples for compression optimization.
# Synced across devices via DHT as a child record of the identity.

struct LearnedSamples {
	schemaVersion @0 :UInt32 = 1;
	chatPatterns @1 :ChatPatterns;
	structuredPatterns @2 :StructuredPatterns;
	lastTraining @3 :UInt64;
}

struct ChatPatterns {
	# Schema version for this struct.
	schemaVersion @0 :UInt32 = 1;

	# Word/n-gram frequency map for training compression dictionaries.
	# Keys are words or n-grams (1-3 words), values are occurrence counts.
	wordFreq @1 :List(FreqEntry);
	
	# Structural templates extracted from messages.
	# e.g., "{sender}:*" for message structure patterns.
	templates @2 :List(Text);
	
	# Emoji frequency map.
	emojiFreq @3 :List(FreqEntry);
	
	# Reserved: message type frequency (text, system, stream_chunk, etc.).
	# Always serialized as a zero-length list; not yet used for training.
	# Do not reuse this field number.
	typeFreq @4 :List(FreqEntry);
	
	# Timestamp of last sample collection.
	lastUpdated @5 :UInt64;
}

struct StructuredPatterns {
	# Schema version for this struct.
	schemaVersion @0 :UInt32 = 1;

	# Template frequency for structured records.
	# Values are replaced with wildcards to preserve privacy.
	# e.g., "username\0*\0dob\0*" from "username\0alice\0dob\019900101"
	templateFreq @1 :List(FreqEntry);
	
	# Key pattern frequency (field names that appear in records).
	keyFreq @2 :List(FreqEntry);
	
	# Timestamp of last sample collection.
	lastUpdated @3 :UInt64;
}

struct FreqEntry {
	key @0 :Text;
	count @1 :UInt64;
}

struct TrainingMetadata {
	schemaVersion @0 :UInt32 = 1;
	lastTraining @1 :UInt64;
}
