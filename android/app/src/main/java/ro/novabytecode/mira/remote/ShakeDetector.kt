package ro.novabytecode.mira.remote

import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import kotlin.math.abs

/**
 * Detects a quick left-right shake: several strong, alternating jolts along the
 * phone's X axis within a short window. Also reports gentle tilt so the robot
 * on screen can look around.
 */
class ShakeDetector(
    private val sensors: SensorManager,
    private val onShake: () -> Unit,
    private val onTilt: (x: Float, y: Float) -> Unit,
) : SensorEventListener {

    private val gravity = FloatArray(3)
    private var lastSign = 0
    private val swings = ArrayDeque<Long>()
    private var cooldownUntil = 0L

    fun start() {
        val accel = sensors.getDefaultSensor(Sensor.TYPE_ACCELEROMETER) ?: return
        sensors.registerListener(this, accel, SensorManager.SENSOR_DELAY_GAME)
    }

    fun stop() = sensors.unregisterListener(this)

    override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {}

    override fun onSensorChanged(event: SensorEvent) {
        val now = event.timestamp / 1_000_000 // ms
        // Low-pass filter isolates gravity (tilt); the rest is the user's motion.
        for (i in 0..2) gravity[i] = 0.9f * gravity[i] + 0.1f * event.values[i]
        val x = event.values[0] - gravity[0]
        val y = event.values[1] - gravity[1]
        val z = event.values[2] - gravity[2]
        onTilt(gravity[0], gravity[1])

        if (now < cooldownUntil) return
        val sideways = abs(x) > THRESHOLD && abs(x) > 1.4f * maxOf(abs(y), abs(z))
        if (!sideways) return
        val sign = if (x > 0) 1 else -1
        if (sign == lastSign) return
        lastSign = sign
        swings.addLast(now)
        while (swings.isNotEmpty() && now - swings.first() > WINDOW_MS) swings.removeFirst()
        if (swings.size >= SWINGS) {
            swings.clear()
            lastSign = 0
            cooldownUntil = now + COOLDOWN_MS
            onShake()
        }
    }

    private companion object {
        const val THRESHOLD = 11f // m/s², a deliberate flick
        const val SWINGS = 4 // left-right-left-right
        const val WINDOW_MS = 900L
        const val COOLDOWN_MS = 4000L
    }
}
