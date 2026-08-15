buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:8.11.0")
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:1.9.25")
    }
}

allprojects {
    repositories {
        google()
        mavenCentral()
    }
}

// Optional escape hatch from cloud-synced working directories.
//
// This repo lives under OneDrive on the maintainer's machine, and OneDrive
// walks the multi-gigabyte Gradle build tree while Gradle is writing it. The
// result is intermittent `AccessDeniedException` / "Unable to delete
// directory ... a process has files open" failures in `mergeNativeLibs` and
// `packageRelease` — nothing to do with the build, and not reproducible on a
// second run.
//
// Set `KITTY_ANDROID_BUILD_DIR` to move every module's build output somewhere
// outside the synced tree (the same trick `CARGO_TARGET_DIR=C:\kt` already
// plays for Rust, which also dodges MAX_PATH). Unset, everything behaves
// exactly as stock. See docs/RELEASE.md.
val externalBuildRoot: String? = System.getenv("KITTY_ANDROID_BUILD_DIR")
if (!externalBuildRoot.isNullOrBlank()) {
    allprojects {
        layout.buildDirectory.set(file("$externalBuildRoot/${project.name}"))
    }
}

tasks.register("clean").configure {
    delete("build")
}

