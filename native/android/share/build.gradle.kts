// Compiled as a Gradle subproject of the dx-generated Android app.
// AGP 9's built-in Kotlin compiles this Android library subproject.
plugins {
    id("com.android.library")
}

// Match the dx-generated app: `compile_sdk` from Dioxus.toml, built against the
// newest installed platform minor (e.g. android-37.2) and build-tools.
evaluationDependsOn(":app")
val app = project(":app").extensions.getByType<com.android.build.api.dsl.ApplicationExtension>()

android {
    namespace = "com.debanjanbasu.po.share"
    compileSdk = app.compileSdk
    compileSdkMinor = app.compileSdkMinor
    buildToolsVersion = app.buildToolsVersion

    defaultConfig {
        minSdk = app.defaultConfig.minSdk
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

}

dependencies {
    // FileProvider for QR images and exported files.
    implementation("androidx.core:core:1.19.1")

    // Veilid's Android keyring backend looks up
    // `androidx.security.crypto.MasterKey` from Rust over JNI, and its
    // ProtectedStore registry component then fails to initialize when the class
    // is absent — `app::retry` classifies that as retryable, so `run_local_init`
    // loops and the UI never leaves its startup splash:
    //
    //   INFO pstore: Secure key storage service unavailable, falling back ...
    //     Keyring generic error: Caught Java exception: Didn't find class
    //     "androidx/security/crypto/MasterKey" on path: DexPathList[...]
    //   ERROR registry: Error initializing component 'ProtectedStore':
    //     Generic: Could not initialize the protected store.
    //
    // Veilid's own Android hosts declare it for exactly this reason
    // (`veilid-core/src/tests/android/**/app/build.gradle`,
    // `veilid-flutter/android/build.gradle`). It is declared here rather than on
    // the app module because dx generates that module's dependency list
    // (`target/dx/<profile>/android/app/app/build.gradle.kts`) and the app does
    // not own it; every module under `native/android/` is copied in as
    // `:plugins:<name>plugin` and is `implementation`-depended on by the app, so
    // a library module's `implementation` dependency lands on the app's runtime
    // classpath and therefore in the APK's dex.
    implementation("androidx.security:security-crypto:1.1.0")
}
