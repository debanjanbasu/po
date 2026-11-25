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
    namespace = "com.debanjanbasu.po.biometric"
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
    implementation("androidx.core:core:1.19.1")
    // Newest is a pre-release (1.4.0-beta01); stay on the latest stable.
    implementation("androidx.biometric:biometric:1.1.0")
    implementation("androidx.fragment:fragment:1.9.1")
}
