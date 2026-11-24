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
        versionCode = 10
        versionName = "0.2.0"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
        externalNativeBuild { cmake { arguments += listOf("-DANDROID_STL=c++_static") } }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    // Release signing comes from the environment (CI secrets or a local shell). A stable key is
    // required so a newer APK can be installed over an older one with `adb install -r`.
    // Without it, fall back to the debug key so local `assembleRelease` still works.
    val releaseKeystore = System.getenv("RD_KEYSTORE_FILE")?.takeIf { it.isNotBlank() }
    if (releaseKeystore != null) {
        signingConfigs {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = System.getenv("RD_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("RD_KEY_ALIAS")
                keyPassword = System.getenv("RD_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName(if (releaseKeystore != null) "release" else "debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_23
        targetCompatibility = JavaVersion.VERSION_23
    }
    kotlinOptions { jvmTarget = "23" }
}
