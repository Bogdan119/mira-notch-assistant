plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "ro.novabytecode.mira.remote"
    compileSdk = 34

    defaultConfig {
        applicationId = "ro.novabytecode.mira.remote"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    // QR scanning through Google Play services: no camera permission needed.
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")
}
