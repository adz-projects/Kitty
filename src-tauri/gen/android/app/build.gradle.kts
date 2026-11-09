import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release signing. `keystore.properties` is gitignored and is *not* in this
// repo — the upload key is the maintainer's to hold, not the build's. See
// docs/RELEASE.md for how to create one. When it is absent (any dev machine,
// any CI job that is only smoke-building) the release variant simply has no
// signing config: `assembleRelease`/`bundleRelease` still produce an unsigned
// artifact, which is what you want for verifying the lane works, and Play
// rejects it, which is what you want for not shipping by accident.
val keystoreProperties = Properties().apply {
    val propFile = rootProject.file("keystore.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}
val hasReleaseKeystore = keystoreProperties.getProperty("storeFile") != null

// A file-only configuration for the LiteRT AAR: resolved just so we can unzip
// its native `libLiteRt.so` into `jniLibs`, never added to the compile
// classpath (its Kotlin API metadata is too new for this project's compiler).
// `isTransitive = false` keeps it to the single AAR file.
val litertAar: Configuration by configurations.creating { isTransitive = false }

// Unpack `jni/<abi>/libLiteRt*.so` from the AAR into a generated jniLibs dir
// that the `main` source set includes (wired in `android { sourceSets }` below).
// AGP then merges these into the APK exactly like our own `libkitty_lib.so`, and
// the Rust side loads `libLiteRt.so` by name at runtime.
val extractLiteRtJni by tasks.registering(Copy::class) {
    from(zipTree(litertAar.singleFile)) {
        include("jni/**/libLiteRt.so", "jni/**/libLiteRtClGlAccelerator.so")
        eachFile { path = path.removePrefix("jni/") }
        includeEmptyDirs = false
    }
    into(layout.buildDirectory.dir("litert-jni"))
}

// The LiteRT-LM runtime with the embedding API, for EmbeddingGemma 2's
// `.litertlm` bundle (`BigTinyV2/daemon/src/litert/lm_embedder.rs`), which the
// Rust side opens as `liblitert-lm.so` at run time. Google publishes the
// Android build only inside the `litert-lm-api` wheel on PyPI, so it is fetched
// pinned by its PyPI sha256 (same pin discipline as `plugins/build.py` uses
// for the Windows DLL) and unzipped into its own generated jniLibs dir.
val litertLmWheelUrl =
    "https://files.pythonhosted.org/packages/f3/13/" +
        "3857f2098ee92f029562039882d2960c7b574fc9021344441d88ed65f17e/" +
        "litert_lm_api-0.19.1-py3-none-android_23_arm64_v8a.whl"
val litertLmWheelSha256 = "605bec7ed6391b7b5b4788d0587f7d3bf73ff6f6750bb3749c9bbed557af0254"
val litertLmWheel = layout.buildDirectory.file("litert-lm-wheel/litert_lm_api-0.19.1-android_arm64.whl")

val fetchLiteRtLmWheel by tasks.registering {
    outputs.file(litertLmWheel)
    doLast {
        val out = litertLmWheel.get().asFile
        fun sha256(f: java.io.File): String =
            java.security.MessageDigest.getInstance("SHA-256")
                .digest(f.readBytes())
                .joinToString("") { "%02x".format(it) }
        if (out.exists() && sha256(out) == litertLmWheelSha256) return@doLast
        out.parentFile.mkdirs()
        java.net.URI(litertLmWheelUrl).toURL().openStream().use { input ->
            out.outputStream().use { input.copyTo(it) }
        }
        val got = sha256(out)
        if (got != litertLmWheelSha256) {
            out.delete()
            throw GradleException("litert-lm-api wheel hash mismatch: got $got, pinned $litertLmWheelSha256")
        }
    }
}

val extractLiteRtLmJni by tasks.registering(Copy::class) {
    dependsOn(fetchLiteRtLmWheel)
    from(zipTree(litertLmWheel)) {
        include("litert_lm/liblitert-lm.so")
        eachFile { path = "arm64-v8a/$name" }
        includeEmptyDirs = false
    }
    into(layout.buildDirectory.dir("litert-lm-jni"))
}

android {
    compileSdk = 36
    namespace = "com.kitty.app"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.kitty.app"
        // 26, not Tauri's default 24: the native library is compiled against
        // the API 26 sysroot (`cargo ndk --platform 26`, docs/ANDROID.md §11),
        // so on 24 or 25 the APK installs and then dies at `System.loadLibrary`
        // with an unresolved-symbol link error. Declaring 26 turns a runtime
        // crash into Play refusing the install, which is the honest outcome.
        minSdk = 26
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (hasReleaseKeystore) {
            create("release") {
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
                storeFile = rootProject.file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            if (hasReleaseKeystore) {
                signingConfig = signingConfigs.getByName("release")
            }
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
    // Pick up the LiteRT `.so` extracted from the AAR (see `extractLiteRtJni`).
    sourceSets["main"].jniLibs.srcDir(layout.buildDirectory.dir("litert-jni"))
    // And the LiteRT-LM embedding runtime (see `extractLiteRtLmJni`).
    sourceSets["main"].jniLibs.srcDir(layout.buildDirectory.dir("litert-lm-jni"))
}

// The extraction must run before AGP merges jniLibs. `preBuild` is early enough
// and variant-agnostic.
tasks.named("preBuild") { dependsOn(extractLiteRtJni, extractLiteRtLmJni) }

rust {
    rootDirRel = "../../../"
}

dependencies {
    // LiteRT runtime, native `.so` ONLY — see the `litertAar` extraction below.
    // We deliberately do NOT put the AAR on the compile classpath
    // (`implementation`): its bundled Kotlin API is compiled with Kotlin
    // metadata 2.3.0, incompatible with this project's 1.9, and breaks the
    // Kotlin compile. The Rust embedder only needs `libLiteRt.so` at runtime.
    add("litertAar", "com.google.ai.edge.litert:litert:2.1.4@aar")
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")