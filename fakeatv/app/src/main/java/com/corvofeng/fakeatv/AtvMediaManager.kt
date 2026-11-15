package com.corvofeng.fakeatv

import android.content.ComponentName
import android.content.Context
import android.graphics.Bitmap
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.io.ByteArrayOutputStream

object AtvMediaManager {
    private const val TAG = "AtvMediaManager"
    private var appContext: Context? = null
    private var activeController: MediaController? = null
    private var mediaSessionManager: MediaSessionManager? = null
    private val mainHandler = Handler(Looper.getMainLooper())

    private val controllerCallback = object : MediaController.Callback() {
        override fun onPlaybackStateChanged(state: PlaybackState?) {
            Log.d(TAG, "onPlaybackStateChanged: state=${state?.state} pos=${state?.position}")
            publishCurrentMedia()
        }

        override fun onMetadataChanged(metadata: MediaMetadata?) {
            Log.d(TAG, "onMetadataChanged: title=${metadata?.getString(MediaMetadata.METADATA_KEY_TITLE)}")
            publishCurrentMedia()
        }

        override fun onSessionDestroyed() {
            Log.d(TAG, "onSessionDestroyed")
            activeController = null
            publishCurrentMedia()
        }
    }

    private val sessionsListener = MediaSessionManager.OnActiveSessionsChangedListener { controllers ->
        Log.d(TAG, "OnActiveSessionsChanged: count=${controllers?.size ?: 0}")
        updateActiveController(controllers?.firstOrNull())
    }

    fun init(context: Context, notificationListenerComponent: ComponentName? = null) {
        appContext = context.applicationContext
        try {
            mediaSessionManager = context.getSystemService(Context.MEDIA_SESSION_SERVICE) as? MediaSessionManager
            val comp = notificationListenerComponent
                ?: ComponentName(context, AtvNotificationListenerService::class.java)

            mediaSessionManager?.let { msm ->
                try {
                    msm.addOnActiveSessionsChangedListener(sessionsListener, comp, mainHandler)
                    val activeSessions = msm.getActiveSessions(comp)
                    updateActiveController(activeSessions.firstOrNull())
                } catch (e: SecurityException) {
                    Log.w(TAG, "No permission for getActiveSessions yet: ${e.message}")
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "Failed to init MediaSessionManager: ${e.message}")
        }

        publishCurrentMedia()
    }

    private fun updateActiveController(newController: MediaController?) {
        if (activeController?.sessionToken == newController?.sessionToken) {
            return
        }
        try {
            activeController?.unregisterCallback(controllerCallback)
        } catch (_: Exception) {}

        activeController = newController
        activeController?.let {
            it.registerCallback(controllerCallback, mainHandler)
            Log.i(TAG, "Attached to active media session: ${it.packageName}")
        }
        publishCurrentMedia()
    }

    fun publishCurrentMedia() {
        val controller = activeController
        val metadata = controller?.metadata
        val state = controller?.playbackState

        val title = metadata?.getString(MediaMetadata.METADATA_KEY_TITLE)
            ?: if (controller != null) "Медиаплеер" else null
        val artist = metadata?.getString(MediaMetadata.METADATA_KEY_ARTIST)
            ?: metadata?.getString(MediaMetadata.METADATA_KEY_ALBUM_ARTIST)
            ?: if (controller != null) controller.packageName else null
        val album = metadata?.getString(MediaMetadata.METADATA_KEY_ALBUM)

        val duration = (metadata?.getLong(MediaMetadata.METADATA_KEY_DURATION) ?: 0L).toDouble() / 1000.0
        val elapsed = (state?.position ?: 0L).toDouble() / 1000.0
        val rate = state?.playbackSpeed ?: 1.0f

        val isPlaying = when (state?.state) {
            PlaybackState.STATE_PLAYING,
            PlaybackState.STATE_FAST_FORWARDING,
            PlaybackState.STATE_REWINDING,
            PlaybackState.STATE_SKIPPING_TO_NEXT,
            PlaybackState.STATE_SKIPPING_TO_PREVIOUS -> true
            else -> false
        }
        val appName = controller?.packageName
        val appBundleId = controller?.packageName

        var artBytes: ByteArray? = null
        try {
            val bitmap = metadata?.getBitmap(MediaMetadata.METADATA_KEY_ART)
                ?: metadata?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART)
            if (bitmap != null) {
                val stream = ByteArrayOutputStream()
                bitmap.compress(Bitmap.CompressFormat.JPEG, 85, stream)
                artBytes = stream.toByteArray()
            }
        } catch (e: Exception) {
            Log.w(TAG, "Failed to compress artwork: ${e.message}")
        }

        try {
            AtvNative.nativeUpdateMediaInfo(
                title,
                artist,
                album,
                appName,
                appBundleId,
                duration,
                elapsed,
                rate,
                isPlaying,
                artBytes
            )
            Log.d(TAG, "Published media: title=$title artist=$artist isPlaying=$isPlaying")
        } catch (e: Exception) {
            Log.w(TAG, "Failed to call nativeUpdateMediaInfo: ${e.message}")
        }
    }

    fun handleMediaCommand(command: String, param: Long) {
        val lower = command.lowercase().trim()
        Log.i(TAG, "handleMediaCommand: $lower (param=$param)")

        val controls = activeController?.transportControls

        when (lower) {
            "play" -> {
                if (controls != null) {
                    controls.play()
                } else {
                    LocalAdbDriver.sendKey("media_play")
                }
            }
            "pause" -> {
                if (controls != null) {
                    controls.pause()
                } else {
                    LocalAdbDriver.sendKey("media_pause")
                }
            }
            "toggle" -> {
                val isPlaying = activeController?.playbackState?.state == PlaybackState.STATE_PLAYING
                if (controls != null) {
                    if (isPlaying) controls.pause() else controls.play()
                } else {
                    LocalAdbDriver.sendKey("play_pause")
                }
            }
            "next" -> {
                if (controls != null) {
                    controls.skipToNext()
                } else {
                    LocalAdbDriver.sendKey("media_next")
                }
            }
            "prev" -> {
                if (controls != null) {
                    controls.skipToPrevious()
                } else {
                    LocalAdbDriver.sendKey("media_previous")
                }
            }
            "stop" -> {
                if (controls != null) {
                    controls.stop()
                } else {
                    LocalAdbDriver.sendKey("media_stop")
                }
            }
            "seek" -> {
                if (controls != null) {
                    controls.seekTo(param * 1000)
                }
            }
            else -> {
                Log.w(TAG, "Unknown media command: $command")
            }
        }
    }
}
