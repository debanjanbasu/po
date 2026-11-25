@0xc7e8a2b4d1f63905;

enum ContactStatus {
    pending @0;
    active @1;
}

struct ChatContact {
    username @0 :Text;
    dhtKey @1 :Text;
    displayName @2 :Text;
    conversationRecordKey @3 :Text;
    status @4 :ContactStatus;
}

struct ChatContactList {
    schemaVersion @0 :UInt32 = 3;
    contacts @1 :List(ChatContact);
    lastUpdated @2 :UInt64;
}

struct ConversationIndex {
    schemaVersion @0 :UInt32 = 1;
    conversations @1 :List(ConversationEntry);
    lastUpdated @2 :UInt64;
}

struct ConversationEntry {
    conversationRecordKey @0 :Text;
    contactUsername @1 :Text;
    title @2 :Text;
    lastMessagePreview @3 :Text;
    lastMessageAt @4 :UInt64;
    unreadCount @5 :UInt32;
}
