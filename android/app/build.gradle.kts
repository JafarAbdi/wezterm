plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

fun policy(name: String): String = providers.gradleProperty(name).get()

val abis = policy("wezterm.abis").split(",")
val repoRoot = rootDir.parentFile

val cargoNative = abis.map { abi ->
    tasks.register<Exec>("cargoNative_${abi.replace('-', '_')}") {
        description = "cross-build $abi libwezterm_android.so into jniLibs through ci/android.sh"
        workingDir = repoRoot
        commandLine("ci/android.sh", "native", abi)
        outputs.upToDateWhen { false }
    }
}

tasks.named("preBuild") { dependsOn(cargoNative) }

android {
    namespace = "org.wezterm.android"
    compileSdk = policy("wezterm.compileSdk").toInt()
    ndkVersion = policy("wezterm.ndkVersion")
    buildToolsVersion = policy("wezterm.buildToolsVersion")

    defaultConfig {
        applicationId = "org.wezterm.android"
        minSdk = policy("wezterm.minSdk").toInt()
        targetSdk = policy("wezterm.targetSdk").toInt()
        versionCode = 1
        versionName = "0.1.0-android01"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    splits {
        abi {
            isEnable = true
            reset()
            include(*abis.toTypedArray())
            isUniversalApk = false
        }
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
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
    buildFeatures {
        buildConfig = true
    }
}

dependencies {
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
}
