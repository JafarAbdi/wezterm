package org.wezterm.android

import android.annotation.TargetApi
import android.content.Context
import android.graphics.Rect
import android.os.Build
import android.text.Editable
import android.text.InputType
import android.text.Selection
import android.view.GestureDetector
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceView
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.ExtractedText
import android.view.inputmethod.ExtractedTextRequest
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.TextAttribute
import java.text.BreakIterator

/**
 * The terminal surface and the input it takes: the IME connection, key
 * presses and touch.
 *
 * Every act becomes one typed native call; the GUI thread turns it into
 * the window event WezTerm's terminal window already handles. What the IME
 * edits lives in the [TerminalInputConnection] it is connected through;
 * only the newest connection changes anything. A key, a paste or a tap
 * first sends that connection's unsent text, then makes it forget what it
 * sent, because the laptop's line may no longer end with it; so does a new
 * [inputTarget]. Losing focus drops unsent text: nothing composed before
 * Back, Home or a dialog is typed later.
 */
class TerminalView(context: Context) : SurfaceView(context) {
    private val inputMethods = context.getSystemService(InputMethodManager::class.java)
    private val gestures = GestureDetector(context, Gestures())
    private var selecting = false
    private var tapped = false

    /** The composing text the terminal shows. UI thread only. */
    private var shownPreedit = ""

    /** The connection the IME edits through; calls through any other change nothing. */
    private var connection: TerminalInputConnection? = null

    /**
     * The pane the shown window's input reaches, as the GUI thread last
     * published it. The text the IME connection sent went to this target;
     * another one makes it forget that text.
     */
    var inputTarget: InputTarget? = null
        set(value) {
            if (value == field) return
            field = value
            connection?.forget()
        }

    /** The spacing accent of a hardware dead key waiting for the next character; 0 for none. */
    private var deadAccent = 0

    /** Ctrl and Alt armed on the key row for the next key or committed character, as `KeyEvent` meta bits. */
    var armedMeta = 0
        private set

    /** Called when [armedMeta] changes. */
    var onArmedChanged: () -> Unit = {}

    /** Whether a laptop pane is shown; without one, the view takes no input and no focus. */
    var acceptsInput = false
        set(value) {
            field = value
            isFocusable = value
            isFocusableInTouchMode = value
            if (!value) {
                selecting = false
                endInput()
                connection = null
            }
        }

    override fun onCheckIsTextEditor() = acceptsInput

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        if (!acceptsInput) return null
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE or EditorInfo.IME_FLAG_NO_ENTER_ACTION
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        endInput()
        return TerminalInputConnection(this).also { connection = it }
    }

    internal fun isCurrent(connection: TerminalInputConnection) = connection === this.connection

    /** Drop unsent text and the record of sent text: what the user composed goes nowhere. */
    private fun endInput() {
        connection?.discard()
        deadAccent = 0
        showPreedit("")
    }

    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        if (!hasWindowFocus) endInput()
    }

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        if (!gainFocus) endInput()
    }

    fun toggleArmed(meta: Int) {
        armedMeta = armedMeta xor meta
        onArmedChanged()
    }

    private fun takeArmed(): Int = armedMeta.also {
        if (it != 0) {
            armedMeta = 0
            onArmedChanged()
        }
    }

    internal fun showPreedit(text: String) {
        if (text == shownPreedit) return
        shownPreedit = text
        NativeApp.nativeInputPreedit(text)
    }

    /**
     * Erase `erase` characters before the laptop's cursor, then type
     * `text` once; the terminal stops showing composing text. The erased
     * characters went to [inputTarget]; the GUI thread refuses the edit if
     * input reaches another target by then. True when the line no longer
     * ends with what was typed: a control character or an armed modifier
     * made it a command.
     */
    internal fun commit(erase: Int, text: String): Boolean {
        val meta = if (text.isEmpty()) 0 else takeArmed()
        shownPreedit = ""
        val target = inputTarget
        NativeApp.nativeInputCommit(erase, target?.pane ?: -1, target?.generation ?: 0, text, meta)
        return meta != 0 || text.any { it < ' ' || it == '\u007f' }
    }

    /** Press `code` (a `KeyEvent.KEYCODE_*`) on the laptop. */
    internal fun press(code: Int) {
        sendKey(KeyEvent(KeyEvent.ACTION_DOWN, code))
    }

    /**
     * Paste the clipboard's text into the shown pane, in order with the
     * input around it: the key row, the IME's paste and the key table's
     * paste keys all read the clipboard here, on the UI thread.
     */
    fun paste() {
        if (!acceptsInput) return
        val text = PlatformRequests.clipboardText(context) ?: return
        connection?.settle()
        NativeApp.nativeInputPaste(text)
    }

    /** A tap, or an accessibility service activating the terminal, opens the soft keyboard. */
    override fun performClick(): Boolean {
        super.performClick()
        if (acceptsInput) {
            requestFocus()
            inputMethods.showSoftInput(this, 0)
        }
        return true
    }

    /** A shown soft keyboard would take a hardware Escape to close itself; the laptop gets it instead. */
    override fun onKeyPreIme(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode != KeyEvent.KEYCODE_ESCAPE || !takesKey(event)) return super.onKeyPreIme(keyCode, event)
        if (event.action == KeyEvent.ACTION_DOWN) sendKey(event)
        return true
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean = sendKey(event) || super.onKeyDown(keyCode, event)

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean = takesKey(event) || super.onKeyUp(keyCode, event)

    /** Back, volume and the like stay with the system; a modifier alone sends nothing. */
    private fun takesKey(event: KeyEvent) = acceptsInput && !event.isSystem && !KeyEvent.isModifierKey(event.keyCode)

    /**
     * The key's character comes from its keyboard layout. Right Alt alone
     * is AltGr when the layout gives the key a character with it; any other
     * Alt reaches the laptop as Alt. A dead key's accent waits for the next
     * character and combines with it, as the layout says.
     */
    private fun sendKey(event: KeyEvent): Boolean {
        if (!takesKey(event)) return false
        connection?.settle()
        var meta = event.metaState or takeArmed()
        val chord = KeyEvent.META_CTRL_MASK or KeyEvent.META_META_MASK
        val altGr = meta and KeyEvent.META_ALT_RIGHT_ON != 0 && meta and KeyEvent.META_ALT_LEFT_ON == 0
        var unicode = if (altGr) event.getUnicodeChar(meta and chord.inv()) else 0
        if (unicode != 0) {
            meta = meta and KeyEvent.META_ALT_MASK.inv()
        } else {
            unicode = event.getUnicodeChar(meta and (chord or KeyEvent.META_ALT_MASK).inv())
        }
        if (NativeApp.nativeIsPasteKey(event.keyCode, unicode, meta)) {
            paste()
            return true
        }
        if (unicode and KeyCharacterMap.COMBINING_ACCENT != 0) {
            deadAccent = unicode and KeyCharacterMap.COMBINING_ACCENT_MASK
            showPreedit(String(Character.toChars(deadAccent)))
            return true
        }
        val accent = deadAccent
        if (accent != 0) {
            deadAccent = 0
            showPreedit("")
            if (unicode != 0) {
                val combined = KeyCharacterMap.getDeadChar(accent, unicode)
                if (combined != 0) unicode = combined else NativeApp.nativeInputKey(0, accent, 0)
            }
        }
        NativeApp.nativeInputKey(event.keyCode, unicode, meta)
        return true
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (!acceptsInput) return false
        if (selecting) {
            when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> touch(GESTURE_SELECT_MOVE, event.x, event.y)
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    selecting = false
                    touch(GESTURE_SELECT_END, event.x, event.y)
                }
            }
            return true
        }
        gestures.onTouchEvent(event)
        if (tapped) {
            tapped = false
            performClick()
        }
        return true
    }

    private fun touch(gesture: Int, x: Float, y: Float, from: Float = 0f, to: Float = 0f) {
        NativeApp.nativeInputTouch(gesture, x, y, from, to)
    }

    private inner class Gestures : GestureDetector.SimpleOnGestureListener() {
        override fun onDown(e: MotionEvent) = true

        override fun onSingleTapUp(e: MotionEvent): Boolean {
            connection?.settle()
            touch(GESTURE_TAP, e.x, e.y)
            tapped = true
            return true
        }

        override fun onScroll(e1: MotionEvent?, e2: MotionEvent, distanceX: Float, distanceY: Float): Boolean {
            val origin = e1 ?: return false
            val offset = e2.y - origin.y
            touch(GESTURE_SCROLL, e2.x, e2.y, from = offset + distanceY, to = offset)
            return true
        }

        override fun onLongPress(e: MotionEvent) {
            connection?.settle()
            selecting = true
            touch(GESTURE_SELECT_START, e.x, e.y)
        }
    }

    companion object {
        /** Gesture codes of `nativeInputTouch`; `wezterm-android/src/input.rs` parses them. */
        const val GESTURE_TAP = 0
        const val GESTURE_SCROLL = 1
        const val GESTURE_SELECT_START = 2
        const val GESTURE_SELECT_MOVE = 3
        const val GESTURE_SELECT_END = 4
    }
}

/**
 * The IME's view of the terminal, one per connection the framework asked
 * for. Its `Editable` is the text the IME edits: [sent], the text the
 * laptop holds before its cursor because this connection typed it, with
 * the IME's edits applied, committed or composing. The IME reads and
 * edits it under the `InputConnection` contract, so `getTextBeforeCursor`,
 * `getExtractedText` and the reported selection describe the same text
 * the edits apply to.
 *
 * When the IME's outermost batch ends, the laptop is brought to
 * [remoteText]: it erases the sent characters after the first difference
 * and types what follows. A difference inside a character erases the
 * whole character and retypes what remains of it. A range that would
 * split a surrogate pair is refused unchanged.
 */
internal class TerminalInputConnection(private val view: TerminalView) : BaseInputConnection(view, true) {
    private val inputMethods = view.context.getSystemService(InputMethodManager::class.java)

    /** What the laptop typed for this connection, after the line it had when the connection last forgot. */
    private var sent = ""
    private var batch = 0

    /** The token of an IME monitoring [getExtractedText]. */
    private var monitor: Int? = null

    override fun beginBatchEdit(): Boolean {
        batch++
        return true
    }

    override fun endBatchEdit(): Boolean {
        if (batch == 0) return false
        if (--batch == 0) apply()
        return batch > 0
    }

    /** One IME edit, applied when its outermost batch ends; refused when invalid or when a newer connection took over. */
    private inline fun edit(valid: Boolean = true, change: () -> Boolean): Boolean {
        if (!view.isCurrent(this) || !valid) return false
        beginBatchEdit()
        try {
            return change()
        } finally {
            endBatchEdit()
        }
    }

    override fun commitText(text: CharSequence?, newCursorPosition: Int) = edit(whole(text)) { super.commitText(text, newCursorPosition) }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int) = edit(whole(text)) { super.setComposingText(text, newCursorPosition) }

    override fun setComposingRegion(start: Int, end: Int) = edit(atCharacters(start, end)) { super.setComposingRegion(start, end) }

    override fun setSelection(start: Int, end: Int) = edit(atCharacters(start, end)) { super.setSelection(start, end) }

    /** Composition ends with its text committed, as the IME contract says. */
    override fun finishComposingText() = edit { super.finishComposingText() }

    @TargetApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
    override fun replaceText(start: Int, end: Int, text: CharSequence, newCursorPosition: Int, textAttribute: TextAttribute?) =
        edit(atCharacters(start, end) && whole(text)) { super.replaceText(start, end, text, newCursorPosition, textAttribute) }

    /** Lengths in UTF-16 units around the selection and composing text, as `BaseInputConnection` deletes them. */
    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int) =
        edit(surroundingAtCharacters(beforeLength, afterLength)) { super.deleteSurroundingText(beforeLength, afterLength) }

    /** `BaseInputConnection` already refuses a range with a broken surrogate pair. */
    override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int) =
        edit { super.deleteSurroundingTextInCodePoints(beforeLength, afterLength) }

    override fun performContextMenuAction(id: Int): Boolean {
        if (id != android.R.id.paste) return super.performContextMenuAction(id)
        if (view.isCurrent(this)) view.paste()
        return true
    }

    override fun getExtractedText(request: ExtractedTextRequest?, flags: Int): ExtractedText? {
        val content = editable ?: return null
        if (flags and GET_EXTRACTED_TEXT_MONITOR != 0) monitor = request?.token
        return extracted(content)
    }

    /** Send what is not sent yet, composing text included, then [forget]. */
    fun settle() {
        val content = editable ?: return
        removeComposingSpans(content)
        apply()
        forget()
    }

    /**
     * The laptop's line may have changed: keep only what was not sent. An
     * edit of sent text that is not on the laptop yet depends on that
     * line, so it goes too.
     */
    fun forget() {
        val content = editable ?: return
        if (!content.startsWith(sent)) return discard()
        content.delete(0, sent.length)
        sent = ""
        report(content)
    }

    /** Drop what was not sent and forget what was. */
    fun discard() {
        val content = editable ?: return
        removeComposingSpans(content)
        content.clear()
        sent = ""
        view.showPreedit("")
        report(content)
    }

    private fun apply() {
        if (!view.isCurrent(this)) return
        val content = editable ?: return
        val text = content.toString()
        val composingStart = getComposingSpanStart(content).let { if (it < 0) it else minOf(it, getComposingSpanEnd(content)) }
        val target = remoteText(sent, text, composingStart)
        val kept = keptLength(sent, target)
        val erase = charactersFrom(sent, kept)
        val typed = target.substring(kept)
        sent = target
        val lineChanged = (erase > 0 || typed.isNotEmpty()) && view.commit(erase, typed)
        view.showPreedit(if (composingStart < 0) "" else text.substring(composingStart))
        if (lineChanged) forget() else report(content)
    }

    private fun report(content: Editable) {
        if (batch > 0) return
        inputMethods.updateSelection(
            view,
            Selection.getSelectionStart(content),
            Selection.getSelectionEnd(content),
            getComposingSpanStart(content),
            getComposingSpanEnd(content),
        )
        monitor?.let { inputMethods.updateExtractedText(view, it, extracted(content)) }
    }

    private fun extracted(content: Editable) = ExtractedText().apply {
        text = content.toString()
        startOffset = 0
        partialStartOffset = -1
        partialEndOffset = -1
        selectionStart = Selection.getSelectionStart(content)
        selectionEnd = Selection.getSelectionEnd(content)
    }

    /** Whether offset `at` of the `Editable` falls between two characters, not inside a surrogate pair. */
    private fun atCharacter(at: Int): Boolean {
        val content = editable ?: return true
        return at <= 0 || at >= content.length || !(Character.isHighSurrogate(content[at - 1]) && Character.isLowSurrogate(content[at]))
    }

    private fun atCharacters(start: Int, end: Int) = atCharacter(start) && atCharacter(end)

    private fun surroundingAtCharacters(beforeLength: Int, afterLength: Int): Boolean {
        val content = editable ?: return true
        var a = Selection.getSelectionStart(content)
        var b = Selection.getSelectionEnd(content)
        if (a > b) a = b.also { b = a }
        val ca = getComposingSpanStart(content)
        val cb = getComposingSpanEnd(content)
        if (ca != -1 && cb != -1) {
            a = minOf(a, ca, cb)
            b = maxOf(b, ca, cb)
        }
        return (beforeLength <= 0 || atCharacter(a - beforeLength)) && (afterLength <= 0 || atCharacter(b + afterLength))
    }

    companion object {
        /**
         * What the laptop should hold before its cursor once the IME's
         * edits are applied: `text`, the IME's text, when nothing composes.
         * While something composes from `composingStart` on, only text
         * committed before the composition that extends `sent` reaches the
         * laptop. Everything else, erasing above all, waits for the
         * composition to end: until then the composition may stand for
         * text the laptop holds (the IME re-marked sent text as composing),
         * and the laptop keeps that text as it is.
         */
        fun remoteText(sent: String, text: String, composingStart: Int): String {
            if (composingStart < 0) return text
            val committed = text.substring(0, composingStart)
            return if (committed.startsWith(sent)) committed else sent
        }

        /** Whether `text` has no lone surrogate. */
        fun whole(text: CharSequence?): Boolean {
            if (text == null) return true
            var i = 0
            while (i < text.length) {
                val c = text[i]
                if (Character.isLowSurrogate(c)) return false
                if (Character.isHighSurrogate(c)) {
                    if (i + 1 >= text.length || !Character.isLowSurrogate(text[i + 1])) return false
                    i++
                }
                i++
            }
            return true
        }

        /** The length of the longest common prefix of `sent` and `text` that ends between two characters of `sent`. */
        fun keptLength(sent: String, text: String): Int {
            var same = 0
            while (same < minOf(sent.length, text.length) && sent[same] == text[same]) same++
            if (same == sent.length) return same
            val characters = BreakIterator.getCharacterInstance().apply { setText(sent) }
            return if (characters.isBoundary(same)) same else characters.preceding(same)
        }

        /** The user-perceived characters of `sent` from offset `from`, which begins one. */
        fun charactersFrom(sent: String, from: Int): Int {
            if (from >= sent.length) return 0
            val characters = BreakIterator.getCharacterInstance().apply { setText(sent) }
            var count = 0
            var at = from
            while (at < sent.length) {
                at = characters.following(at)
                count++
            }
            return count
        }
    }
}
