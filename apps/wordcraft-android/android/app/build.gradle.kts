import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
}

// The Rust library (`apps/wordcraft-android`, one directory up) is built by cargo-ndk for each variant and
// packaged as that variant's native libraries.
val rustAbis: List<String> = providers.gradleProperty("folio.abis").get().split(',').map(String::trim).filter(String::isNotEmpty)
val crateDir: File = rootDir.parentFile
val minSdkVersion = 29

android {
    namespace = "site.lutfi.foliowriter"
    compileSdk {
        version = release(36)
    }
    // The NDK cargo-ndk builds with, also used here to strip the release library.
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "site.lutfi.foliowriter"
        minSdk = minSdkVersion
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

/** Builds the Rust library with cargo-ndk into [outputDir], laid out as `<abi>/libwordcraft_android.so`. */
abstract class CargoNdk @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:Internal
    abstract val crateDir: DirectoryProperty

    @get:Input
    abstract val abis: ListProperty<String>

    /** A profile from `apps/wordcraft-android/.cargo/config.toml`. */
    @get:Input
    abstract val profile: Property<String>

    @get:Input
    abstract val platform: Property<Int>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    init {
        // Cargo knows what changed; it's quick when nothing did.
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val out = outputDir.get().asFile
        out.deleteRecursively()
        out.mkdirs()
        exec.exec {
            workingDir = crateDir.get().asFile
            commandLine(
                listOf(cargo(), "ndk", "--platform", platform.get().toString()) +
                    abis.get().flatMap { listOf("--target", it) } +
                    listOf("--output-dir", out.path, "build", "--locked", "--package", "wordcraft-android", "--profile", profile.get()),
            )
        }
    }

    /** Android Studio started from a desktop launcher may not have `~/.cargo/bin` on its PATH. */
    private fun cargo(): String {
        val home = File(System.getProperty("user.home"), ".cargo/bin/cargo")
        return System.getenv("CARGO")?.takeIf { it.isNotBlank() } ?: if (home.canExecute()) home.path else "cargo"
    }
}

androidComponents {
    onVariants { variant ->
        val debug = variant.buildType != "release"
        val cargoTask = tasks.register<CargoNdk>("cargoNdk${variant.name.replaceFirstChar(Char::uppercaseChar)}") {
            crateDir.set(rootDir.parentFile)
            abis.set(rustAbis)
            profile.set(if (debug) "android-debug" else "android-release")
            platform.set(minSdkVersion)
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoTask, CargoNdk::outputDir)
        if (debug) {
            // Keep the library's symbol table, and install it as a file of its own, so a panic's backtrace in
            // logcat names functions.
            variant.packaging.jniLibs.keepDebugSymbols.add("**/libwordcraft_android.so")
            variant.packaging.jniLibs.useLegacyPackaging.set(true)
        }
    }
}

dependencies {
    implementation(libs.androidx.appcompat)
    implementation(libs.androidx.core)
    implementation(libs.androidx.games.activity)
}
