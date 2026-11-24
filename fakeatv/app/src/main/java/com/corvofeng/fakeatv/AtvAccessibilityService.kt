package com.corvofeng.fakeatv

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Context
import android.content.Intent
import android.graphics.Path
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.util.DisplayMetrics
import android.util.Log
import android.view.KeyEvent
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

class AtvAccessibilityService : AccessibilityService() {

    companion object {
        private const val TAG = "AtvAccessibility"
        var instance: AtvAccessibilityService? = null
            private set

        val isServiceRunning: Boolean
            get() = instance != null
    }

    private var screenWidth = 1920
    private var screenHeight = 1080
    private var pointerX = 960f
    private var pointerY = 540f
    @Volatile
    private var lastEditableNode: AccessibilityNodeInfo? = null
    @Volatile
    private var isKeyboardFocused = false
    @Volatile
    private var lastNotifiedText = ""

    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        menuBindingButton = DeviceInfo.getMenuButtonBinding(this)
        Log.i(TAG, "AtvAccessibilityService connected, menuBindingButton=$menuBindingButton")

        val wm = getSystemService(Context.WINDOW_SERVICE) as? WindowManager
        if (wm != null) {
            val metrics = DisplayMetrics()
            @Suppress("DEPRECATION")
            wm.defaultDisplay.getMetrics(metrics)
            screenWidth = metrics.widthPixels
            screenHeight = metrics.heightPixels
            pointerX = screenWidth / 2f
            pointerY = screenHeight / 2f
            Log.i(TAG, "Screen resolution: ${screenWidth}x${screenHeight}")
        }

        // Auto-start AtvService if not already running
        if (!AtvService.isRunning) {
            val deviceName = DeviceInfo.getDeviceName(this)
            val serviceIntent = Intent(this, AtvService::class.java).apply {
                action = AtvService.ACTION_START
                putExtra(AtvService.EXTRA_NAME, deviceName)
                putExtra(AtvService.EXTRA_PIN, 1111)
            }
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    startForegroundService(serviceIntent)
                } else {
                    startService(serviceIntent)
                }
                Log.i(TAG, "AtvService auto-started from AtvAccessibilityService")
            } catch (e: Exception) {
                Log.e(TAG, "Failed to start AtvService from AtvAccessibilityService", e)
            }
        }
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        val type = event?.eventType ?: return
        val source = event.source

        if (source != null) {
            if ((type == AccessibilityEvent.TYPE_VIEW_FOCUSED ||
                        type == AccessibilityEvent.TYPE_VIEW_CLICKED ||
                        type == AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED) &&
                source.isEditable
            ) {
                lastEditableNode?.recycle()
                lastEditableNode = AccessibilityNodeInfo.obtain(source)
                val currentText = source.text?.toString() ?: ""
                val title = source.hintText?.toString()
                    ?: source.contentDescription?.toString()
                    ?: "Ввод текста"
                if (!isKeyboardFocused || currentText != lastNotifiedText) {
                    isKeyboardFocused = true
                    lastNotifiedText = currentText
                    try {
                        AtvNative.nativeSetKeyboardFocus(true, currentText, title)
                    } catch (_: Throwable) {}
                    Log.d(TAG, "Editable node focused for RTI text input (title=$title)")
                }
            } else if (type == AccessibilityEvent.TYPE_VIEW_FOCUSED && !source.isEditable) {
                if (isKeyboardFocused) {
                    isKeyboardFocused = false
                    lastNotifiedText = ""
                    try {
                        AtvNative.nativeSetKeyboardFocus(false, "", "")
                    } catch (_: Throwable) {}
                    Log.d(TAG, "Editable focus lost -> notified RTI _tiStopped")
                }
            }
            source.recycle()
        } else if (type == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED && isKeyboardFocused) {
            val focusedInput = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            if (focusedInput == null || !focusedInput.isEditable) {
                isKeyboardFocused = false
                lastNotifiedText = ""
                try {
                    AtvNative.nativeSetKeyboardFocus(false, "", "")
                } catch (_: Throwable) {}
            }
            focusedInput?.recycle()
        }
    }

    override fun onInterrupt() {
        Log.w(TAG, "AtvAccessibilityService interrupted")
    }

    override fun onDestroy() {
        super.onDestroy()
        lastEditableNode?.recycle()
        lastEditableNode = null
        instance = null
        Log.i(TAG, "AtvAccessibilityService destroyed")
    }

    fun setFocusedText(text: String): Boolean {
        if (text.isEmpty()) return false

        val focusedNode = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            ?: rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            ?: lastEditableNode

        if (focusedNode == null || !focusedNode.isEditable) {
            Log.w(TAG, "No editable focused node for text input")
            return false
        }

        val arguments = Bundle().apply {
            putCharSequence(
                AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                text
            )
        }

        val success = focusedNode.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, arguments)
        if (success) {
            Log.i(TAG, "Text inserted into focused editable node")
            lastEditableNode?.recycle()
            lastEditableNode = AccessibilityNodeInfo.obtain(focusedNode)
        } else {
            Log.w(TAG, "Failed to insert text into editable node")
        }
        return success
    }

    @Volatile
    var menuBindingButton: String = "none"

    private fun dispatchMediaKey(keyCode: Int): Boolean {
        val am = getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return false
        val down = KeyEvent(KeyEvent.ACTION_DOWN, keyCode)
        val up = KeyEvent(KeyEvent.ACTION_UP, keyCode)
        am.dispatchMediaKeyEvent(down)
        am.dispatchMediaKeyEvent(up)
        return true
    }

    private fun adjustVolume(direction: Int): Boolean {
        val am = getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return false
        am.adjustSuggestedStreamVolume(direction, AudioManager.USE_DEFAULT_STREAM_TYPE, AudioManager.FLAG_SHOW_UI)
        return true
    }

    fun sendKey(name: String, isMappedToMenu: Boolean = false): Boolean {
        val lowerName = name.lowercase().trim()
        val mappedMenu = isMappedToMenu || (menuBindingButton.isNotBlank() &&
                menuBindingButton != "none" &&
                menuBindingButton.equals(lowerName, ignoreCase = true))

        if (mappedMenu) {
            Log.d(TAG, "Button $name redirected to KEYCODE_MENU")
            return performGlobalAction(GLOBAL_ACTION_BACK)
        }

        return when (lowerName) {
            "menu" -> performGlobalAction(GLOBAL_ACTION_BACK)
            "home" -> performGlobalAction(GLOBAL_ACTION_HOME)
            "power" -> performGlobalAction(GLOBAL_ACTION_POWER_DIALOG)
            "select" -> {
                val focused = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
                    ?: findFocus(AccessibilityNodeInfo.FOCUS_ACCESSIBILITY)
                if (focused != null && focused.performAction(AccessibilityNodeInfo.ACTION_CLICK)) {
                    Log.d(TAG, "Clicked focused node")
                    true
                } else {
                    tap(screenWidth / 2f, screenHeight / 2f)
                    true
                }
            }
            "up" -> navigateDpad(AccessibilityNodeInfo.FOCUS_INPUT, "KEYCODE_DPAD_UP", 0, -200)
            "down" -> navigateDpad(AccessibilityNodeInfo.FOCUS_INPUT, "KEYCODE_DPAD_DOWN", 0, 200)
            "left" -> navigateDpad(AccessibilityNodeInfo.FOCUS_INPUT, "KEYCODE_DPAD_LEFT", -200, 0)
            "right" -> navigateDpad(AccessibilityNodeInfo.FOCUS_INPUT, "KEYCODE_DPAD_RIGHT", 200, 0)
            "play_pause" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE)
            "volume_up" -> adjustVolume(AudioManager.ADJUST_RAISE)
            "volume_down" -> adjustVolume(AudioManager.ADJUST_LOWER)
            "mute" -> adjustVolume(AudioManager.ADJUST_TOGGLE_MUTE)
            "siri" -> execFrameworkKeyEvent("KEYCODE_SEARCH")
            else -> execFrameworkKeyEvent("KEYCODE_${lowerName.uppercase()}")
        }
    }

    fun handleButton(name: String) {
        sendKey(name)
    }

    private fun navigateDpad(focusType: Int, fallbackKeycode: String, swipeDx: Int, swipeDy: Int): Boolean {
        val focused = findFocus(focusType)
        val direction = when (fallbackKeycode) {
            "KEYCODE_DPAD_UP" -> android.view.View.FOCUS_UP
            "KEYCODE_DPAD_DOWN" -> android.view.View.FOCUS_DOWN
            "KEYCODE_DPAD_LEFT" -> android.view.View.FOCUS_LEFT
            "KEYCODE_DPAD_RIGHT" -> android.view.View.FOCUS_RIGHT
            else -> 0
        }

        if (focused != null && direction != 0) {
            val next = focused.focusSearch(direction)
            if (next != null && next.performAction(AccessibilityNodeInfo.ACTION_FOCUS)) {
                return true
            }
        }

        // Perform swipe gesture instead of fork-exec
        val cx = screenWidth / 2f
        val cy = screenHeight / 2f
        swipe(cx, cy, cx + swipeDx, cy + swipeDy, 100)
        return true
    }

    private fun execFrameworkKeyEvent(keycode: String): Boolean {
        return when (keycode) {
            "KEYCODE_MEDIA_PLAY_PAUSE" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE)
            "KEYCODE_MEDIA_PLAY" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_PLAY)
            "KEYCODE_MEDIA_PAUSE" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_PAUSE)
            "KEYCODE_MEDIA_NEXT" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_NEXT)
            "KEYCODE_MEDIA_PREVIOUS" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_PREVIOUS)
            "KEYCODE_MEDIA_STOP" -> dispatchMediaKey(KeyEvent.KEYCODE_MEDIA_STOP)
            "KEYCODE_VOLUME_UP" -> adjustVolume(AudioManager.ADJUST_RAISE)
            "KEYCODE_VOLUME_DOWN" -> adjustVolume(AudioManager.ADJUST_LOWER)
            "KEYCODE_VOLUME_MUTE" -> adjustVolume(AudioManager.ADJUST_TOGGLE_MUTE)
            "KEYCODE_MENU", "KEYCODE_BACK" -> performGlobalAction(GLOBAL_ACTION_BACK)
            "KEYCODE_HOME" -> performGlobalAction(GLOBAL_ACTION_HOME)
            "KEYCODE_POWER" -> performGlobalAction(GLOBAL_ACTION_POWER_DIALOG)
            "KEYCODE_SEARCH" -> {
                try {
                    val intent = Intent(Intent.ACTION_ASSIST).apply {
                        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    }
                    startActivity(intent)
                    true
                } catch (_: Exception) {
                    false
                }
            }
            "KEYCODE_DPAD_CENTER" -> {
                val focused = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
                    ?: findFocus(AccessibilityNodeInfo.FOCUS_ACCESSIBILITY)
                if (focused != null && focused.performAction(AccessibilityNodeInfo.ACTION_CLICK)) {
                    true
                } else {
                    tap(screenWidth / 2f, screenHeight / 2f)
                    true
                }
            }
            else -> false
        }
    }

    fun handleTouch(dx: Double, dy: Double, phase: Int) {
        if (phase == 2) { // Moved
            val sensitivity = 1.5f
            pointerX = (pointerX + dx.toFloat() * sensitivity).coerceIn(0f, screenWidth.toFloat())
            pointerY = (pointerY + dy.toFloat() * sensitivity).coerceIn(0f, screenHeight.toFloat())
        }
    }

    fun handleMouseClick() {
        tap(pointerX, pointerY)
    }

    private fun tap(x: Float, y: Float) {
        val path = Path().apply {
            moveTo(x, y)
        }
        val stroke = GestureDescription.StrokeDescription(path, 0, 50)
        val gesture = GestureDescription.Builder().addStroke(stroke).build()
        dispatchGesture(gesture, null, null)
    }

    private fun swipe(startX: Float, startY: Float, endX: Float, endY: Float, durationMs: Long) {
        val path = Path().apply {
            moveTo(startX, startY)
            lineTo(endX, endY)
        }
        val stroke = GestureDescription.StrokeDescription(path, 0, durationMs)
        val gesture = GestureDescription.Builder().addStroke(stroke).build()
        dispatchGesture(gesture, null, null)
    }
}
