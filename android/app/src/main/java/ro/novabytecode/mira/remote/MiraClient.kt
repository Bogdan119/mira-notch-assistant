package ro.novabytecode.mira.remote

import android.net.Uri
import android.os.Handler
import android.os.Looper
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors

/** Where the laptop is and the pairing code it showed in its QR. */
data class Pairing(val host: String, val port: Int, val code: String) {
    companion object {
        /** Parses `mira://192.168.1.23:47800?code=123456`. */
        fun parse(text: String): Pairing? {
            val uri = Uri.parse(text.trim())
            if (uri.scheme != "mira") return null
            val host = uri.host ?: return null
            val code = uri.getQueryParameter("code") ?: return null
            val port = if (uri.port > 0) uri.port else 47800
            if (code.length != 6 || code.any { !it.isDigit() }) return null
            return Pairing(host, port, code)
        }
    }
}

/** Talks to the laptop's Mira over the local network. Callbacks run on the main thread. */
class MiraClient {
    private val io = Executors.newSingleThreadExecutor()
    private val main = Handler(Looper.getMainLooper())

    fun ping(p: Pairing, done: (Result) -> Unit) = call(p, "GET", "/ping", done)

    fun shake(p: Pairing, done: (Result) -> Unit) = call(p, "POST", "/shake", done)

    enum class Result { OK, BAD_CODE, LOCKED_OUT, UNREACHABLE }

    private fun call(p: Pairing, method: String, path: String, done: (Result) -> Unit) {
        io.execute {
            val result = try {
                val conn = URL("http", p.host, p.port, path).openConnection() as HttpURLConnection
                conn.requestMethod = method
                conn.connectTimeout = 2500
                conn.readTimeout = 2500
                conn.setRequestProperty("X-Mira-Code", p.code)
                if (method == "POST") {
                    conn.doOutput = true
                    conn.setFixedLengthStreamingMode(0)
                    conn.outputStream.close()
                }
                when (conn.responseCode) {
                    200 -> Result.OK
                    401 -> Result.BAD_CODE
                    429 -> Result.LOCKED_OUT
                    else -> Result.UNREACHABLE
                }.also { conn.disconnect() }
            } catch (e: Exception) {
                Result.UNREACHABLE
            }
            main.post { done(result) }
        }
    }
}
