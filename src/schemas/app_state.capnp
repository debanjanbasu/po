@0xdc8225bf8f1ee3ce;

struct AppState {
    schemaVersion @0 :UInt32 = 1;
    lastActiveRoute @1 :Text;
    navBarLabels @2 :List(Text);
    theme @3 :Text;
    retentionPolicy @4 :UInt8 = 0;
}
