import java.nio.file.Files
import java.nio.file.attribute.PosixFilePermission

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

fun policy(name: String): String = providers.gradleProperty(name).get()

val abis = policy("wezterm.abis").split(",")
val repoRoot = rootDir.parentFile

for (variant in listOf("debug", "release")) {
    val cargoNative = abis.map { abi ->
        tasks.register<Exec>("cargoNative_${variant}_${abi.replace('-', '_')}") {
            workingDir = repoRoot
            commandLine("ci/android.sh", "native", abi, variant)
            outputs.upToDateWhen { false }
        }
    }
    tasks.matching { it.name == "pre${variant.replaceFirstChar { it.uppercase() }}Build" }
        .configureEach { dependsOn(cargoNative) }
}

val notices = tasks.register<Exec>("androidNotices") {
    workingDir = repoRoot
    commandLine("uv", "run", "--no-project", "python", "ci/android_notices.py")
    outputs.upToDateWhen { false }
}
tasks.named("preBuild") { dependsOn(notices) }

val signingRequested = gradle.startParameter.taskNames.any { it.contains("Release", ignoreCase = true) }
fun signingInput(name: String): String = providers.environmentVariable(name).orNull
    ?.takeIf { it.isNotBlank() } ?: error("Missing release signing input $name")
fun privateSigningFile(name: String): File {
    val file = File(signingInput(name)).canonicalFile
    require(file.isFile && !file.toPath().startsWith(repoRoot.toPath())) { "$name must be a file outside the repository" }
    val permissions = Files.getPosixFilePermissions(file.toPath())
    require(permissions == setOf(PosixFilePermission.OWNER_READ, PosixFilePermission.OWNER_WRITE)) {
        "$name must have mode 0600"
    }
    return file
}
gradle.taskGraph.whenReady {
    require(signingRequested || allTasks.none { it.name.contains("Release", ignoreCase = true) }) {
        "Release tasks require explicit release signing inputs and a Release task invocation"
    }
}

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

    val releaseTests = providers.gradleProperty("wezterm.releaseTests").orNull == "true"
    testBuildType = if (releaseTests) "release" else "debug"
    sourceSets {
        if (releaseTests) getByName("androidTest").java.setSrcDirs(listOf("src/releaseTest/java"))
        getByName("main") {
            jniLibs.setSrcDirs(emptyList<String>())
            assets.srcDir(repoRoot.resolve("target/android-notices/assets"))
        }
        getByName("debug").jniLibs.srcDir("src/main/jniLibs")
        getByName("release").jniLibs.srcDir(repoRoot.resolve("target/android-release/jniLibs"))
    }

    signingConfigs {
        if (signingRequested) {
            create("delivery") {
                storeFile = privateSigningFile("WEZTERM_ANDROID_KEYSTORE")
                storePassword = privateSigningFile("WEZTERM_ANDROID_STORE_PASSWORD_FILE").readText().trimEnd('\r', '\n')
                keyAlias = signingInput("WEZTERM_ANDROID_KEY_ALIAS")
                keyPassword = privateSigningFile("WEZTERM_ANDROID_KEY_PASSWORD_FILE").readText().trimEnd('\r', '\n')
                require(!storePassword.isNullOrEmpty() && !keyPassword.isNullOrEmpty()) { "Empty signing password file" }
                enableV1Signing = false
                enableV2Signing = true
            }
        }
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
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "release.pro")
            testProguardFile("release-test.pro")
            if (signingRequested) signingConfig = signingConfigs.getByName("delivery")
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
