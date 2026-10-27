package ro.novabytecode.mira.remote

import android.app.Activity
import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.hardware.SensorManager
import android.os.Build
import android.os.Bundle
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning

/**
 * Mira Remote: pair with the laptop by scanning the QR code Mira shows
 * (right-click the robot → "Connect your phone"), then shake the phone
 * left-right to send the robot to sleep and lock the laptop.
 */
class MainActivity : Activity() {
    private val client = MiraClient()
    private lateinit var pet: PetView
    private lateinit var status: TextView
    private lateinit var shake: ShakeDetector
    private var pairing: Pairing? = null

    private val prefs by lazy { getSharedPreferences("mira", Context.MODE_PRIVATE) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val dp = resources.displayMetrics.density

        pet = PetView(this)
        status = TextView(this).apply {
            setTextColor(0xFF9A9AA0.toInt())
            textSize = 15f
            gravity = Gravity.CENTER
        }
        val title = TextView(this).apply {
            text = "Mira"
            setTextColor(Color.WHITE)
            textSize = 30f
            typeface = Typeface.DEFAULT_BOLD
            gravity = Gravity.CENTER
        }
        val hint = TextView(this).apply {
            text = "Scutură telefonul rapid stânga–dreapta ca să adormi robotul și să blochezi laptopul."
            setTextColor(0xFF6E6E76.toInt())
            textSize = 13f
            gravity = Gravity.CENTER
        }
        val scan = pillButton("Scanează codul QR", 0xFF5B8CFF.toInt()) { scanQr() }
        val test = pillButton("Trimite acum", 0xFF26262E.toInt()) { send() }
        val manual = pillButton("Introdu manual", 0xFF1C1C22.toInt()) { askManual() }

        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding((24 * dp).toInt(), (32 * dp).toInt(), (24 * dp).toInt(), (24 * dp).toInt())
            addView(title)
            addView(pet, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(status, spaced(dp))
            addView(hint, spaced(dp))
            addView(scan, spaced(dp))
            addView(test, spaced(dp))
            addView(manual, spaced(dp))
        }
        setContentView(column)

        shake = ShakeDetector(
            getSystemService(Context.SENSOR_SERVICE) as SensorManager,
            onShake = { send() },
            onTilt = { x, y -> pet.tilt(x, y) },
        )
        pairing = prefs.getString("pairing", null)?.let(Pairing::parse)
        refresh()
    }

    override fun onResume() {
        super.onResume()
        shake.start()
        refresh()
    }

    override fun onPause() {
        super.onPause()
        shake.stop()
    }

    private fun spaced(dp: Float) = LinearLayout.LayoutParams(
        LinearLayout.LayoutParams.MATCH_PARENT,
        LinearLayout.LayoutParams.WRAP_CONTENT,
    ).apply { topMargin = (12 * dp).toInt() }

    private fun pillButton(label: String, fill: Int, onClick: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        textSize = 16f
        setTextColor(Color.WHITE)
        stateListAnimator = null
        background = GradientDrawable().apply {
            cornerRadius = 100f
            setColor(fill)
        }
        val pad = (14 * resources.displayMetrics.density).toInt()
        setPadding(pad, pad, pad, pad)
        setOnClickListener { onClick() }
    }

    private fun scanQr() {
        val options = GmsBarcodeScannerOptions.Builder()
            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
            .build()
        GmsBarcodeScanning.getClient(this, options).startScan()
            .addOnSuccessListener { code -> use(code.rawValue ?: "") }
            .addOnFailureListener {
                status.text = "Scanerul nu e disponibil — folosește „Introdu manual”."
            }
    }

    private fun askManual() {
        val input = EditText(this).apply {
            hint = "mira://192.168.1.23:47800?code=123456"
            inputType = InputType.TYPE_TEXT_VARIATION_URI
            setText(pairing?.let { "mira://${it.host}:${it.port}?code=${it.code}" } ?: "")
        }
        android.app.AlertDialog.Builder(this)
            .setTitle("Adresa din fereastra Mira")
            .setView(input)
            .setPositiveButton("OK") { _, _ -> use(input.text.toString()) }
            .setNegativeButton("Anulează", null)
            .show()
    }

    private fun use(text: String) {
        val p = Pairing.parse(text)
        if (p == null) {
            status.text = "Codul QR nu e de la Mira."
            return
        }
        pairing = p
        prefs.edit().putString("pairing", "mira://${p.host}:${p.port}?code=${p.code}").apply()
        refresh()
    }

    private fun refresh() {
        val p = pairing
        if (p == null) {
            pet.setConnected(false)
            status.text = "Neconectat. Pe laptop: click dreapta pe robot → Conectează telefonul (QR)."
            return
        }
        status.text = "Mă conectez la ${p.host}…"
        client.ping(p) { r ->
            pet.setConnected(r == MiraClient.Result.OK)
            status.text = describe(r, "Conectat la laptop · ${p.host}")
        }
    }

    private fun send() {
        val p = pairing ?: run {
            status.text = "Scanează întâi codul QR de pe laptop."
            return
        }
        buzz()
        pet.playShake()
        client.shake(p) { r -> status.text = describe(r, "Noapte bună, laptop! 💤") }
    }

    private fun describe(r: MiraClient.Result, ok: String) = when (r) {
        MiraClient.Result.OK -> ok
        MiraClient.Result.BAD_CODE -> "Cod greșit — scanează din nou QR-ul."
        MiraClient.Result.LOCKED_OUT -> "Prea multe încercări, mai așteaptă un minut."
        MiraClient.Result.UNREACHABLE ->
            "Nu găsesc laptopul. Ești pe același Wi-Fi și Mira e pornită?"
    }

    @Suppress("DEPRECATION")
    private fun buzz() {
        val vibrator = if (Build.VERSION.SDK_INT >= 31) {
            (getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager).defaultVibrator
        } else {
            getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
        }
        vibrator.vibrate(VibrationEffect.createWaveform(longArrayOf(0, 40, 60, 40), -1))
    }
}
