package ro.novabytecode.mira.remote

import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.view.View
import android.view.animation.AccelerateDecelerateInterpolator
import kotlin.math.max
import kotlin.math.min
import kotlin.math.sin

/**
 * The robot on the phone: the same dark pill with blue eyes as on the laptop.
 * Its eyes follow the phone's tilt; after a shake it glances left, right,
 * then sleepily closes its eyes — a preview of what the laptop does.
 */
class PetView(context: Context) : View(context) {
    private val density = resources.displayMetrics.density
    private val body = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xFF1C1C22.toInt() }
    private val eye = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xFF8FB0FF.toInt() }
    private val rect = RectF()

    private var tiltX = 0f
    private var tiltY = 0f
    private var lookX = 0f
    private var lookY = 0f
    private var offsetX = 0f
    private var squash = 0f
    private var lid = 0f
    private var connected = false
    private var show: ValueAnimator? = null
    private val startTime = System.nanoTime()

    fun setConnected(value: Boolean) {
        connected = value
        invalidate()
    }

    /** Gravity components, m/s²: eyes drift the way the phone leans. */
    fun tilt(x: Float, y: Float) {
        tiltX = (-x / 9.8f).coerceIn(-1f, 1f)
        tiltY = (y / 9.8f - 0.6f).coerceIn(-1f, 1f)
        if (show == null) invalidate()
    }

    /** Left, right, then close the eyes. */
    fun playShake() {
        show?.cancel()
        show = ValueAnimator.ofFloat(0f, 1f).apply {
            duration = 2600
            interpolator = AccelerateDecelerateInterpolator()
            addUpdateListener {
                val t = it.animatedValue as Float * 2.6f
                when {
                    t < 0.6f -> { offsetX = -ease(t / 0.6f) * 0.28f; lookX = -ease(t / 0.4f) }
                    t < 1.4f -> {
                        val k = ease((t - 0.6f) / 0.8f)
                        offsetX = -0.28f + 0.56f * k; lookX = -1f + 2f * k
                    }
                    t < 1.9f -> { offsetX = 0.28f * (1f - ease((t - 1.4f) / 0.5f)); lookX = 1f - ease((t - 1.4f) / 0.5f) }
                    else -> { offsetX = 0f; lookX = 0f; lid = ease((t - 1.9f) / 0.6f) }
                }
                squash = sin(t * 9f) * 0.06f * (1f - min(1f, t / 1.9f))
                invalidate()
            }
            addListener(object : android.animation.AnimatorListenerAdapter() {
                override fun onAnimationEnd(animation: android.animation.Animator) {
                    postDelayed({ lid = 0f; show = null; invalidate() }, 900)
                }
            })
            start()
        }
    }

    private fun ease(t: Float): Float {
        val x = t.coerceIn(0f, 1f)
        return x * x * (3 - 2 * x)
    }

    override fun onDraw(canvas: Canvas) {
        val w = width.toFloat()
        val h = height.toFloat()
        val bw = min(w * 0.62f, 320 * density)
        val bh = bw * 0.27f
        // Idle: a slow breathing bob.
        val t = (System.nanoTime() - startTime) / 1e9f
        val bob = if (show == null) sin(t * 2f) * 2 * density else 0f
        val cx = w / 2 + offsetX * w
        val cy = h / 2 + bob
        val sx = 1f + squash
        val sy = 1f - squash
        rect.set(cx - bw / 2 * sx, cy - bh / 2 * sy, cx + bw / 2 * sx, cy + bh / 2 * sy)
        canvas.drawRoundRect(rect, bh / 2, bh / 2, body)

        if (show == null) {
            lookX += (tiltX - lookX) * 0.25f
            lookY += (tiltY - lookY) * 0.25f
        }
        val r = bh * 0.17f
        val gap = bh * 0.38f
        val open = max(0.08f, 1f - lid)
        eye.alpha = if (connected) 255 else 140
        for (side in listOf(-1f, 1f)) {
            val ex = cx + side * gap + lookX * r * 1.2f
            val ey = cy + lookY * r * 0.6f + r * 0.3f * lid
            canvas.drawOval(ex - r, ey - r * open, ex + r, ey + r * open, eye)
        }
        if (show == null) postInvalidateOnAnimation()
    }
}
