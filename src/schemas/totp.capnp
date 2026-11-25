@0xb1a1d8e3f2c4a6b5;

struct TotpToken {
  issuer @0 :Text;
  accountName @1 :Text;
  secret @2 :Text;
  algorithm @3 :Text;
  digits @4 :UInt32;
  period @5 :UInt32;
  createdAt @6 :UInt64;
  updatedAt @7 :UInt64;
}

struct TotpTokenList {
  schemaVersion @0 :UInt32 = 1;
  tokens @1 :List(TotpToken);
  lastUpdated @2 :UInt64;
}
