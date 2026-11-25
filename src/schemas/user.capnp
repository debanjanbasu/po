@0xdf8a2b851534999e;

struct EncryptedBlob {
  ciphertext @0 :Data;
  nonce @1 :Data;
}

struct User {
  schemaVersion @0 :UInt32 = 2;
  username @1 :Text;
  keyPair @2 :EncryptedBlob;
  dhtKey @3 :Text;
  dob @4 :Int32;           # YYYYMMDD integer
  pinHash @5 :Text;        # Argon2 PHC string
  termsAccepted @6 :Bool;
  biometricsEnabled @7 :Bool;
  accountId @8 :Text;
  cryptoVersion @9 :UInt32 = 0; # 0 = legacy account-agnostic key; 2 = account-bound DHT key
}
