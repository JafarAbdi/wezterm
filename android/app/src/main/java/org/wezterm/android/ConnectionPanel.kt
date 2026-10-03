package org.wezterm.android

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.graphics.Color
import android.text.InputType
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView

/**
 * The connection screen: the profile form, the attach progress with its
 * Cancel button, the failure, empty, cancelled and disconnected states
 * with Connect or Reconnect, and the dialog for the prompt the native
 * connection waits on.
 *
 * It renders [NativeApp.connectionStatus] and holds no connection state of
 * its own. A prompt belongs to the native side until it is answered with
 * its attempt and prompt ids; dismissing the dialog because the Activity
 * stops answers nothing, and the next [render] shows the prompt again.
 * Cancel, Disconnect and Reconnect act on the attempt the last [render]
 * showed; the native side refuses them for any other.
 */
class ConnectionPanel(private val activity: Activity, private val pickIdentity: () -> Unit) {
    internal val host = field(R.string.connect_host, InputType.TYPE_TEXT_VARIATION_URI or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS)
    internal val port = field(R.string.connect_port, InputType.TYPE_CLASS_NUMBER)
    internal val user = field(R.string.connect_user, InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS)
    internal val remoteWezterm = field(R.string.connect_remote_wezterm, InputType.TYPE_TEXT_VARIATION_URI or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS)
    internal val identity = TextView(activity)

    /** What the last key import did. [render] leaves it alone. */
    internal val importOutcome = TextView(activity)
    internal val importIdentity = Button(activity).apply {
        setText(R.string.identity_import)
        setOnClickListener {
            importOutcome.text = ""
            pickIdentity()
        }
    }
    internal val connect = Button(activity).apply {
        setText(R.string.connect_button)
        setOnClickListener { connect() }
    }
    internal val cancel = Button(activity).apply {
        setText(R.string.connect_cancel)
        setOnClickListener { NativeApp.nativeCancelConnect(shownAttempt) }
    }
    internal val disconnect = Button(activity).apply {
        setText(R.string.connection_disconnect)
        setOnClickListener { NativeApp.nativeDisconnect(shownAttempt) }
    }
    internal val message = TextView(activity).apply { setTextIsSelectable(true) }

    /** The attempt the last [render] showed. */
    private var shownAttempt = 0L
    internal var promptDialog: AlertDialog? = null
    internal var promptInput: EditText? = null

    /** Attempt and prompt id of the prompt [promptDialog] shows. */
    private var shownPrompt: Pair<Long, Long>? = null
    private val form: LinearLayout
    val view: View

    init {
        val wrap = ViewGroup.LayoutParams.WRAP_CONTENT
        val fill = ViewGroup.LayoutParams.MATCH_PARENT
        form = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            for (child in listOf(host, port, user, remoteWezterm, identity, importOutcome, importIdentity, connect)) {
                addView(child, LinearLayout.LayoutParams(fill, wrap))
            }
        }
        val column = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 48, 48, 48)
            addView(TextView(activity).apply { setText(R.string.connect_title); textSize = 20f }, LinearLayout.LayoutParams(fill, wrap))
            addView(message, LinearLayout.LayoutParams(fill, wrap))
            addView(cancel, LinearLayout.LayoutParams(fill, wrap))
            addView(disconnect, LinearLayout.LayoutParams(fill, wrap))
            addView(form, LinearLayout.LayoutParams(fill, wrap))
        }
        view = ScrollView(activity).apply {
            setBackgroundColor(Color.BLACK)
            isFillViewport = true
            addView(column, ViewGroup.LayoutParams(fill, wrap))
        }
        val saved = activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        host.setText(saved.getString("host", ""))
        port.setText(saved.getString("port", ""))
        user.setText(saved.getString("user", ""))
        remoteWezterm.setText(saved.getString("remote_wezterm", ""))
    }

    private fun field(hint: Int, variation: Int) = EditText(activity).apply {
        setHint(hint)
        isSingleLine = true
        inputType = if (variation == InputType.TYPE_CLASS_NUMBER) variation else InputType.TYPE_CLASS_TEXT or variation
    }

    private fun connect() {
        val profile = Profile(host.text.toString(), port.text.toString(), user.text.toString(), remoteWezterm.text.toString())
        for (input in listOf(host, port, user, remoteWezterm)) input.error = null
        when (val outcome = NativeApp.connect(profile)) {
            is ConnectOutcome.Started -> activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit()
                .putString("host", profile.host.trim())
                .putString("port", profile.port.trim())
                .putString("user", profile.user.trim())
                .putString("remote_wezterm", profile.remoteWezterm.trim())
                .apply()
            is ConnectOutcome.InvalidProfile -> when (outcome.field) {
                "host" -> host
                "port" -> port
                "user" -> user
                else -> remoteWezterm
            }.let {
                it.error = outcome.message
                it.requestFocus()
            }
            is ConnectOutcome.Refused -> message.text = outcome.message
        }
    }

    /** Show `status`. `terminalVisible` hides the panel: a window is bound, or this is the diagnostic applet. */
    fun render(status: ConnectionStatus, terminalVisible: Boolean) {
        shownAttempt = status.attempt
        val attachedWithWindows = status.phase == "attached" && status.windows > 0
        view.visibility = if (terminalVisible || attachedWithWindows) View.GONE else View.VISIBLE
        val editable = status.phase in listOf("idle", "failed", "cancelled", "disconnected")
        form.visibility = if (editable) View.VISIBLE else View.GONE
        cancel.visibility = if (status.phase == "attaching") View.VISIBLE else View.GONE
        disconnect.visibility = if (status.phase == "attached") View.VISIBLE else View.GONE
        connect.setText(if (status.phase == "disconnected") R.string.connect_reconnect else R.string.connect_button)
        connect.isEnabled = status.ready && !status.closing
        identity.setText(if (status.identity) R.string.identity_present else R.string.identity_absent)
        message.text = when (status.phase) {
            "attaching" -> activity.getString(R.string.connection_attaching, status.progress)
            "cancelling" -> activity.getString(R.string.connection_cancelling)
            "cancelled" -> activity.getString(R.string.connection_cancelled)
            "attached" -> activity.getString(R.string.connection_empty)
            "disconnecting" -> activity.getString(R.string.connection_disconnecting)
            "disconnected" -> activity.getString(
                if (status.cause == "user") R.string.connection_disconnected else R.string.connection_lost,
            )
            "failing", "failed" -> activity.getString(failureHeading(status.failureKind)) + "\n\n" + status.failureMessage
            else -> ""
        }
        showPrompt(status)
    }

    private fun failureHeading(kind: String) = when (kind) {
        "unreachable" -> R.string.failure_unreachable
        "host_key_rejected" -> R.string.failure_host_key_rejected
        "host_key_changed" -> R.string.failure_host_key_changed
        "authentication" -> R.string.failure_authentication
        "authentication_cancelled" -> R.string.failure_authentication_cancelled
        "server_unavailable" -> R.string.failure_server_unavailable
        "incompatible_version" -> R.string.failure_incompatible_version
        "engine_ended" -> R.string.failure_engine_ended
        else -> R.string.failure_other
    }

    private fun showPrompt(status: ConnectionStatus) {
        val prompt = status.prompt
        val wanted = prompt?.let { status.attempt to it.id }
        if (wanted == shownPrompt && (wanted == null || promptDialog?.isShowing == true)) return
        dismissPrompt()
        if (prompt == null) return
        val attempt = status.attempt
        val builder = AlertDialog.Builder(activity)
        when (prompt) {
            is ConnectionPrompt.HostTrust -> builder
                .setTitle(R.string.prompt_host_trust_title)
                .setMessage(activity.getString(R.string.prompt_host_trust_message, prompt.remoteAddress, prompt.fingerprint))
                .setPositiveButton(R.string.prompt_trust) { _, _ -> NativeApp.nativeAnswerHostTrust(attempt, prompt.id, true) }
                .setNegativeButton(R.string.prompt_reject) { _, _ -> NativeApp.nativeAnswerHostTrust(attempt, prompt.id, false) }
                .setOnCancelListener { NativeApp.nativeAnswerHostTrust(attempt, prompt.id, false) }
                .setNeutralButton(R.string.prompt_stop) { _, _ -> NativeApp.nativeCancelConnect(attempt) }
            is ConnectionPrompt.Secret -> textPrompt(builder, attempt, prompt.id, prompt.text, secret = true)
            is ConnectionPrompt.Text -> textPrompt(builder, attempt, prompt.id, prompt.text, secret = false)
        }
        promptDialog = builder.show().apply { setCanceledOnTouchOutside(false) }
        shownPrompt = wanted
    }

    private fun textPrompt(builder: AlertDialog.Builder, attempt: Long, id: Long, text: String, secret: Boolean) {
        val input = EditText(activity).apply {
            isSingleLine = true
            inputType = InputType.TYPE_CLASS_TEXT or
                if (secret) InputType.TYPE_TEXT_VARIATION_PASSWORD else InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        }
        promptInput = input
        builder
            .setTitle(R.string.prompt_secret_title)
            .setMessage(text)
            .setView(input)
            .setPositiveButton(R.string.prompt_ok) { _, _ ->
                NativeApp.nativeAnswerText(attempt, id, input.text.toString())
                input.text.clear()
            }
            .setNegativeButton(R.string.prompt_cancel) { _, _ -> NativeApp.nativeAnswerText(attempt, id, null) }
            .setNeutralButton(R.string.prompt_stop) { _, _ ->
                input.text.clear()
                NativeApp.nativeCancelConnect(attempt)
            }
            .setOnCancelListener { NativeApp.nativeAnswerText(attempt, id, null) }
    }

    /**
     * Take the dialog down without answering and wipe what was typed into
     * it; the prompt stays pending on the native side.
     */
    fun dismissPrompt() {
        promptInput?.text?.clear()
        promptDialog?.dismiss()
        promptDialog = null
        promptInput = null
        shownPrompt = null
    }

    companion object {
        const val PREFERENCES = "profile"
    }
}
