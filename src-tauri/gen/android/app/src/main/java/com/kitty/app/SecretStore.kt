package com.kitty.app

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Android's half of CLAUDE.md rule 4 — "secrets never touch JS or plaintext
 * disk" — for provider API keys and BigTiny's at-rest encryption key.
 *
 * Why this exists at all: `keyring` 3.6.3, which covers Windows Credential
 * Manager on the desktop side, has **no Android backend**. Its feature list is
 * apple-native / linux-native / windows-native and nothing else, so on
 * `aarch64-linux-android` the crate silently falls through to its catch-all
 * `pub use mock as default`. That compiles, and stores every secret in a
 * process-lifetime HashMap: keys appeared to save and were gone after a
 * relaunch (D24). `docs/ANDROID.md` originally planned this as "enable
 * keyring's Android backend" — there is no such backend to enable.
 *
 * Design: values are AES-256-GCM sealed with a key that is generated inside
 * the AndroidKeyStore and never leaves it — this process can ask for
 * encrypt/decrypt, but cannot read the key material, and on a device with a
 * TEE or StrongBox the key is not in the application processor's memory at
 * all. The sealed blobs then sit in an ordinary private SharedPreferences
 * file. App-private storage alone would already be behind the kernel's UID
 * sandbox and file-based encryption; the Keystore layer is what additionally
 * defends against an offline extraction of that file, which is the realistic
 * threat for a stolen or rooted device.
 *
 * Blob layout: `iv (12 bytes) || ciphertext || GCM tag`, Base64 (NO_WRAP).
 * A fresh IV per write, taken from the Cipher rather than chosen here —
 * reusing an IV under GCM is catastrophic, and letting the provider pick it
 * removes the opportunity to get that wrong.
 */
object SecretStore {
    private const val PREFS = "kitty_secrets"
    private const val KEY_ALIAS = "kitty_secret_store"
    private const val ANDROID_KEYSTORE = "AndroidKeyStore"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_TAG_BITS = 128
    private const val IV_BYTES = 12

    /** The AndroidKeyStore key, created on first use and reused forever after.
     *
     * Deliberately *not* `setUserAuthenticationRequired(true)`: Kitty reads
     * provider keys from a background health loop and from scheduled tasks
     * that fire with the app closed, and an auth-gated key would make those
     * fail with no one present to authenticate. The key is still hardware-held
     * and non-exportable. */
    private fun secretKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        val existing = keyStore.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry
        if (existing != null) return existing.secretKey

        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return generator.generateKey()
    }

    private fun prefs(ctx: Context) =
        ctx.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** Seal and store. Throws on any crypto or Keystore failure — the caller
     *  must surface that rather than pretend the write happened. */
    fun set(ctx: Context, account: String, value: String) {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, secretKey())
        val sealed = cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        val blob = cipher.iv + sealed
        // `commit`, not `apply`: `apply` returns before the write reaches
        // disk, and a secret that is only in the in-memory prefs cache when
        // the process dies is exactly the bug this class exists to fix.
        val ok = prefs(ctx).edit()
            .putString(account, Base64.encodeToString(blob, Base64.NO_WRAP))
            .commit()
        if (!ok) throw IllegalStateException("could not persist the secret to disk")
    }

    /** `null` when nothing is stored under [account]. Throws when something
     *  *is* stored and could not be read back — the two are different answers
     *  and the Rust side depends on telling them apart (a transient failure
     *  read as "not configured" is what silently disabled a tool server once
     *  already; see `classify_read_result`). */
    fun get(ctx: Context, account: String): String? {
        val stored = prefs(ctx).getString(account, null) ?: return null
        val blob = Base64.decode(stored, Base64.NO_WRAP)
        if (blob.size <= IV_BYTES) {
            throw IllegalStateException("stored secret is truncated")
        }
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(
            Cipher.DECRYPT_MODE,
            secretKey(),
            GCMParameterSpec(GCM_TAG_BITS, blob, 0, IV_BYTES)
        )
        return String(cipher.doFinal(blob, IV_BYTES, blob.size - IV_BYTES), Charsets.UTF_8)
    }

    fun delete(ctx: Context, account: String) {
        prefs(ctx).edit().remove(account).commit()
    }
}
