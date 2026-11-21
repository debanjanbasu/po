@0xb7c9e2d1f4a68053;

struct LocalConversation {
  schemaVersion @0 :UInt32 = 3;
  manifest @1 :Data;
  roster @2 :Data;
  keyRing @3 :Data;
  convKeyBytes @4 :Data;
  myFeedMeta @5 :Data;
  messagePage @6 :Data;
}
