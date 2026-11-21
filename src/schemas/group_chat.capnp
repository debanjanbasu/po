@0xa1b2c3d4e5f67890;

enum MemberRole {
  owner @0;
  member @1;
}

enum MemberStatus {
  invited @0;
  active @1;
}

enum MessageStatus {
  sent @0;
  pending @1;
  failed @2;
}

struct ConversationManifest {
  schemaVersion @0 :UInt32 = 3;
  title @1 :Text;
  ownerIdentityKey @2 :Text;
  createdAt @3 :UInt64;
}

struct MemberEntry {
  username @0 :Text;
  identityKey @1 :Text;
  role @2 :MemberRole;
  status @3 :MemberStatus;
}

struct ConversationRoster {
  schemaVersion @0 :UInt32 = 3;
  members @1 :List(MemberEntry);
  lastUpdated @2 :UInt64;
}

struct KeyRingEntry {
  identityKey @0 :Text;
  wrappedKey @1 :Data;
}

struct ConversationKeyRing {
  schemaVersion @0 :UInt32 = 1;
  ownerIdentityKey @1 :Text;
  entries @2 :List(KeyRingEntry);
}

struct ParticipantFeedMeta {
  schemaVersion @0 :UInt32 = 1;
  lastSeq @1 :UInt64;
  lastMessageAt @2 :UInt64;
  archiveKey @3 :Text; # Writable tail, not the history traversal root.
  headPageStartSeq @4 :UInt64;
  # Chain generation of the current archive record (0 = first archive).
  # Scopes the derived archive owner keypair.
  archiveGeneration @5 :UInt32;
  archiveRootKey @6 :Text;
  # Wall-clock epoch-ms stamped when a NEW rollover begins (generation resets
  # to 0). 0 = pre-epoch steady state; equal-epoch merges keep generation
  # arbitration, unequal epochs make generation numbers incomparable.
  archiveEpoch @7 :UInt64;
}

struct FeedMessage {
  seq @0 :UInt64;
  senderUsername @1 :Text;
  content @2 :Text;
  timestamp @3 :UInt64;
  msgId @4 :Text;
  status @5 :MessageStatus;
}
struct MessagePage {
  schemaVersion @0 :UInt32 = 3;
  messages @1 :List(FeedMessage);
  pageSeqStart @2 :UInt64;
  pageSeqEnd @3 :UInt64;
}
