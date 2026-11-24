plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.thunderlight.remote.display"
    compileSdk = 35
    // Pinned so local and CI builds use the same toolchain (sdkmanager "ndk;26.3.11579264").
    ndkVersion = "26.3.11579264"

    defaultConfig {
        applicationId = "com.thunderlight.remote.display"
        minSdk = 26 // dispatchGesture continueStroke
        targetSdk = 35
        // Keep in sync with COMPANION_VERSION_CODE in crates/protocol/src/lib.rs
        versionCode = 9
        versionName = "0.1.0"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
        externalNativeBuild { cmake { arguments += listOf("-DANDROID_STL=c++_static") } }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // The APK is sideloaded by the desktop app; sign with the debug key so
            // `assembleRelease` output is installable without a keystore.
            signingConfig = signingConfigs.getByName("debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}
