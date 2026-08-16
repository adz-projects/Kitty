# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# --- Kitty's Tauri Android plugin -------------------------------------------
# The release build is minified (isMinifyEnabled=true). Tauri dispatches plugin
# calls by REFLECTION: `run_mobile_plugin("setSecret", {account, value})` looks
# up an @Command method by name and deserializes the payload into an @InvokeArg
# class by field name. R8 renames both by default, so in a minified build the
# call fails ("could not store the secret" / a lateinit arg never set) even
# though debug — which isn't minified — works. That is exactly the "creating a
# provider fails to store API tokens" bug: it only ever reproduced in release.
#
# `:tauri-android`'s consumer rules keep the framework, not this app's own
# plugin, so we keep it here. Over-keeping a handful of glue classes costs
# almost nothing and is the only correct behavior for reflection targets.
-keep class com.kitty.app.KittyPlugin { *; }
-keep class com.kitty.app.SecretStore { *; }
-keep class com.kitty.app.SecretArgs { *; }
-keep class com.kitty.app.DownloadNoticeArgs { *; }

# Belt-and-braces for any future plugin/arg class: keep every @Command method
# and every field of an @InvokeArg / @TauriPlugin class, whatever it's named.
-keepclassmembers class * {
  @app.tauri.annotation.Command <methods>;
}
-keep @app.tauri.annotation.TauriPlugin class * { *; }
-keep @app.tauri.annotation.InvokeArg class * { *; }