@0xdf8a2b851534aaa0;

enum OpType {
  profile @0;
  security @1;
  profileAndSecurity @2;
  appState @3;
  totp @4;
  chat @5;
  chatContacts @6;
  createIdentity @7;
  reencryptSubkeys @8;
  dictPublish @9;
  convFeedSync @10;
  convRecordCreate @11;
  convInviteSignal @12;
}

struct PendingOperation {
	schemaVersion @0 :UInt32 = 1;
	id @1 :UInt64;
	username @2 :Text;
	opType @3 :OpType;
	payload @4 :Data;
	createdAt @5 :Int64;
	sequenceNumber @6 :UInt64;
	attempts @7 :UInt32;
	lastError @8 :Text;
	scheduledAt @9 :UInt64;
	targetKey @10 :Text;
	identityKey @11 :Text;
	# Second half of a two-part target. An invite retry is addressed to a
	# peer AND scoped to one conversation; the two are separate fields
	# because a record key contains colons of its own, so a single
	# delimiter-joined string cannot be split safely.
	targetSubkey @12 :Text;
}
