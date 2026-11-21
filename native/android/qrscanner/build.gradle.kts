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
    namespace = "com.debanjanbasu.oo.qrscanner"
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
    // The scanner module itself is delivered by Play services; this is only
    // needed at compile time for the availability probe in `canScan()`
    // (`GoogleApiAvailability`). Already resolved transitively through
    // `play-services-code-scanner`.
    implementation("com.google.android.gms:play-services-base:18.11.1")
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")
}
