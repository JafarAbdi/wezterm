package org.wezterm.android

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.os.SystemClock
import android.util.Log
import android.view.Gravity
import android.view.KeyEvent
import android.view.SurfaceHolder
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import java.io.IOException
import java.io.InputStream
import java.util.concurrent.atomic.AtomicLong
import kotlin.concurrent.thread

/**
 * Hosts the terminal `SurfaceView` and forwards its lifecycle to the GUI
 * thread as generation-tagged events.
 *
 * `surfaceDestroyed` blocks until the native side has released the surface,
 * as the platform contract requires; the GUI thread never calls back into
 * this thread, so the wait cannot deadlock, and an engine that ends
 * resolves the wait from its shutdown.
 *
 * The surface fills what the system bars and the soft keyboard leave
 * free, above a row of keys a soft keyboard lacks; its size is the size of
 * the laptop pane, which every client of that pane shares.
 *
 * Leaving this Activity (Back, Home, rotation, finish) only takes the
 * surface away. Logical windows and their panes live in the native engine
 * for the life of the process; nothing here closes one. The selector shows
 * the native window list as it is at that moment and keeps no copy.
 */
class TerminalActivity : Activity(), SurfaceHolder.Callback, PlatformRequests.Listener {
    private var generation = 0L
    internal lateinit var surfaceView: TerminalView
    internal lateinit var keys: LinearLayout
    internal lateinit var selector: TextView
    internal lateinit var engineBanner: TextView
    internal var selectorDialog: AlertDialog? = null
    internal lateinit var connection: ConnectionPanel

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engine = NativeApp.startTerminal(
            this,
            intent.getStringExtra(EXTRA_CONFIG_OVERRIDES) ?: "",
            intent.getBooleanExtra(EXTRA_DIAGNOSTIC_APPLET, false),
        )
        Log.i(TAG, "engine $engine")
        connection = ConnectionPanel(this) {
            val pick = Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*")
            @Suppress("DEPRECATION")
            startActivityForResult(pick, REQUEST_IDENTITY)
        }
        surfaceView = TerminalView(this)
        surfaceView.holder.addCallback(this)
        keys = keyRow(surfaceView)
        selector = overlayText().apply {
            visibility = View.GONE
            setOnClickListener { showSelector() }
        }
        engineBanner = overlayText().apply { visibility = View.GONE }
        val container = FrameLayout(this)
        val fill = ViewGroup.LayoutParams.MATCH_PARENT
        val wrap = ViewGroup.LayoutParams.WRAP_CONTENT
        val terminal = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(surfaceView, LinearLayout.LayoutParams(fill, 0, 1f))
            addView(keys, LinearLayout.LayoutParams(fill, wrap))
        }
        container.addView(terminal, FrameLayout.LayoutParams(fill, fill))
        container.addView(connection.view, FrameLayout.LayoutParams(fill, fill))
        container.addView(selector, FrameLayout.LayoutParams(wrap, wrap, Gravity.TOP or Gravity.END))
        container.addView(engineBanner, FrameLayout.LayoutParams(fill, wrap, Gravity.BOTTOM))
        container.setOnApplyWindowInsetsListener { _, insets ->
            @Suppress("DEPRECATION")
            container.setPadding(
                insets.systemWindowInsetLeft,
                insets.systemWindowInsetTop,
                insets.systemWindowInsetRight,
                insets.systemWindowInsetBottom,
            )
            insets
        }
        setContentView(container)
    }

    /** Escape, one-shot Ctrl and Alt, Tab, the arrows and Paste. */
    private fun keyRow(terminal: TerminalView): LinearLayout {
        val row = LinearLayout(this).apply { setBackgroundColor(Color.BLACK) }
        fun key(label: Int, description: Int, action: () -> Unit) = Button(this).apply {
            setText(label)
            contentDescription = getString(description)
            isAllCaps = false
            minWidth = 0
            minimumWidth = 0
            setPadding(0, 0, 0, 0)
            setOnClickListener { action() }
            row.addView(this, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        }
        key(R.string.key_escape, R.string.key_escape_description) { terminal.press(KeyEvent.KEYCODE_ESCAPE) }
        val ctrl = key(R.string.key_ctrl, R.string.key_ctrl_description) { terminal.toggleArmed(KeyEvent.META_CTRL_ON) }
        val alt = key(R.string.key_alt, R.string.key_alt_description) { terminal.toggleArmed(KeyEvent.META_ALT_ON) }
        key(R.string.key_tab, R.string.key_tab_description) { terminal.press(KeyEvent.KEYCODE_TAB) }
        key(R.string.key_left, R.string.key_left_description) { terminal.press(KeyEvent.KEYCODE_DPAD_LEFT) }
        key(R.string.key_down, R.string.key_down_description) { terminal.press(KeyEvent.KEYCODE_DPAD_DOWN) }
        key(R.string.key_up, R.string.key_up_description) { terminal.press(KeyEvent.KEYCODE_DPAD_UP) }
        key(R.string.key_right, R.string.key_right_description) { terminal.press(KeyEvent.KEYCODE_DPAD_RIGHT) }
        key(R.string.key_paste, R.string.key_paste_description) { terminal.paste() }
        terminal.onArmedChanged = {
            ctrl.isSelected = terminal.armedMeta and KeyEvent.META_CTRL_ON != 0
            alt.isSelected = terminal.armedMeta and KeyEvent.META_ALT_ON != 0
            for (modifier in listOf(ctrl, alt)) modifier.alpha = if (modifier.isSelected) 1f else ARMABLE_ALPHA
        }
        terminal.onArmedChanged()
        return row
    }

    private fun overlayText() = TextView(this).apply {
        setTextColor(Color.WHITE)
        setBackgroundColor(Color.argb(0xC0, 0x30, 0x30, 0x30))
        setPadding(24, 16, 24, 16)
    }

    override fun onStart() {
        super.onStart()
        PlatformRequests.listener = this
        onWindowsChanged()
        onConnectionChanged()
        showEngineEnd()
    }

    override fun onStop() {
        PlatformRequests.listener = null
        selectorDialog?.dismiss()
        connection.dismissPrompt()
        super.onStop()
    }

    override fun onConnectionChanged() {
        connection.render(NativeApp.connectionStatus(), terminalVisible = NativeApp.diagnosticApplet)
        val shown = connection.view.visibility == View.GONE
        surfaceView.acceptsInput = shown
        keys.visibility = if (shown) View.VISIBLE else View.GONE
    }

    /**
     * The document the user picked as SSH key: its bytes go to app-private
     * storage through the native side and are wiped here. Nothing is logged.
     */
    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        val document = data?.data
        if (requestCode != REQUEST_IDENTITY || resultCode != RESULT_OK || document == null) return
        thread(name = "wezterm-identity-import") {
            val key = try {
                // One byte more than a key file may have, so an oversized document is refused natively.
                contentResolver.openInputStream(document)?.use { readAtMost(it, MAX_KEY_FILE_SIZE + 1) }
            } catch (e: IOException) {
                null
            } catch (e: SecurityException) {
                null
            }
            val refusal = key?.let { NativeApp.nativeImportIdentity(it) }
            key?.fill(0)
            runOnUiThread {
                val text = when {
                    refusal == null -> getString(R.string.identity_unreadable)
                    refusal.isEmpty() -> getString(R.string.identity_imported)
                    else -> getString(R.string.identity_refused, refusal.substringAfter(": "))
                }
                connection.importOutcome.text = text
                onConnectionChanged()
            }
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        moveTaskToBack(true)
    }

    override fun onWindowsChanged() {
        val status = NativeApp.surfaceStatus()
        surfaceView.inputTarget = status.inputTarget
        val bound = status.windows.indexOfFirst { it.id == status.boundWindow }
        selector.visibility = if (status.windows.size > 1) View.VISIBLE else View.GONE
        selector.text = getString(R.string.window_selector, bound + 1, status.windows.size)
    }

    override fun onEngineEnded() {
        showEngineEnd()
        onConnectionChanged()
    }

    private fun showEngineEnd() {
        val status = NativeApp.surfaceStatus()
        if (status.engine != "failed" && status.engine != "stopped") return
        engineBanner.text = getString(R.string.engine_ended, status.engine, status.engineMessage)
        engineBanner.visibility = View.VISIBLE
    }

    private fun showSelector() {
        val status = NativeApp.surfaceStatus()
        val windows = status.windows
        val labels = windows.map { it.title.ifEmpty { getString(R.string.window_untitled, it.id) } }.toTypedArray()
        selectorDialog = AlertDialog.Builder(this)
            .setTitle(R.string.window_selector_title)
            .setSingleChoiceItems(labels, windows.indexOfFirst { it.id == status.boundWindow }) { dialog, which ->
                forward("select window") { NativeApp.nativeSelectWindow(windows[which].id) }
                dialog.dismiss()
            }
            .show()
    }

    private fun forward(what: String, event: () -> Unit) {
        try {
            event()
        } catch (e: RuntimeException) {
            Log.e(TAG, "$what refused: ${e.message}")
            showEngineEnd()
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        generation = SurfaceGenerations.next()
        val frame = holder.surfaceFrame
        forward("surface created") { NativeApp.nativeSurfaceCreated(generation, holder.surface, frame.width(), frame.height()) }
        Log.i(TAG, "surface created generation=$generation ${frame.width()}x${frame.height()}")
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        forward("surface changed") { NativeApp.nativeSurfaceChanged(generation, width, height) }
        Log.i(TAG, "surface changed generation=$generation ${width}x$height format=$format")
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        val started = SystemClock.elapsedRealtime()
        val released = NativeApp.nativeSurfaceDestroyed(generation)
        val waitMs = SystemClock.elapsedRealtime() - started
        Log.i(TAG, "surface destroyed generation=$generation released=$released waitMs=$waitMs")
    }

    companion object {
        const val TAG = "WezTermSurface"

        /** Intent extra with `key=value` config override lines; debug builds only. */
        const val EXTRA_CONFIG_OVERRIDES = "org.wezterm.android.CONFIG_OVERRIDES"

        /**
         * Boolean intent extra: open the diagnostic applet instead of the
         * connection screen. Debug builds only, and only on the launch that
         * starts the engine.
         */
        const val EXTRA_DIAGNOSTIC_APPLET = "org.wezterm.android.DIAGNOSTIC_APPLET"

        private const val REQUEST_IDENTITY = 1

        /** How the Ctrl and Alt keys look while not armed. */
        private const val ARMABLE_ALPHA = 0.6f

        /** OpenSSH's `MAX_KEY_FILE_SIZE`; the native import enforces it. */
        private const val MAX_KEY_FILE_SIZE = 1024 * 1024
    }
}

/**
 * The first `limit` bytes of `input`, or all of it when shorter. A read of
 * zero bytes breaks the `InputStream` contract and is an error rather than
 * a retry. `InputStream.readNBytes` needs API 33; minSdk is 24. The working
 * buffer is wiped; the caller wipes the copy.
 */
internal fun readAtMost(input: InputStream, limit: Int): ByteArray {
    val buffer = ByteArray(limit)
    var filled = 0
    try {
        while (filled < limit) {
            val read = input.read(buffer, filled, limit - filled)
            if (read < 0) break
            if (read == 0) throw IOException("the document returned no bytes")
            filled += read
        }
        return buffer.copyOf(filled)
    } finally {
        buffer.fill(0)
    }
}

/** Process-wide, strictly increasing surface generations. */
object SurfaceGenerations {
    private val next = AtomicLong(1)

    fun next(): Long = next.getAndIncrement()
}
