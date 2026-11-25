package com.corvofeng.fakeatv

import android.content.ComponentName
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.util.LruCache
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors

object AtvMediaManager {
    private const val TAG = "AtvMediaManager"
    private var appContext: Context? = null
    private var activeController: MediaController? = null
    private var mediaSessionManager: MediaSessionManager? = null
    private val mainHandler = Handler(Looper.getMainLooper())
    private val mediaExecutor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "AtvMediaPublish").apply { isDaemon = true }
    }
    private val artworkExecutor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "AtvArtworkFetch").apply { isDaemon = true }
    }
    private val artworkUriCache = object : LruCache<String, ByteArray>(20) {}

    @Volatile private var lastPublishedKey: String = ""
    @Volatile private var lastPublishedAtMs: Long = 0L
    @Volatile private var cachedArtKey: String = ""
    @Volatile private var cachedArtBytes: ByteArray? = null
    @Volatile private var pendingArtUri: String? = null

    private val controllerCallback = object : MediaController.Callback() {
        override fun onPlaybackStateChanged(state: PlaybackState?) {
            Log.d(TAG, "onPlaybackStateChanged: state=${state?.state} pos=${state?.position}")
            publishCurrentMedia()
        }

        override fun onMetadataChanged(metadata: MediaMetadata?) {
            Log.d(TAG, "onMetadataChanged: title=${metadata?.getString(MediaMetadata.METADATA_KEY_TITLE)}")
            publishCurrentMedia(force = true)
        }

        override fun onSessionDestroyed() {
            Log.d(TAG, "onSessionDestroyed")
            activeController = null
            publishCurrentMedia(force = true)
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

        publishCurrentMedia(force = true)
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
        publishCurrentMedia(force = true)
    }

    private fun computeLivePositionMs(state: PlaybackState?): Long {
        if (state == null) return 0L
        val base = state.position.coerceAtLeast(0L)
        if (state.state == PlaybackState.STATE_PLAYING && state.lastPositionUpdateTime > 0L) {
            val delta = (SystemClock.elapsedRealtime() - state.lastPositionUpdateTime).coerceAtLeast(0L)
            val speed = if (state.playbackSpeed > 0f) state.playbackSpeed else 1.0f
            return base + (delta * speed).toLong()
        }
        return base
    }

    private fun compressSquareArtwork(bitmap: Bitmap): ByteArray? {
        val targetDim = 512
        val width = bitmap.width
        val height = bitmap.height
        if (width <= 0 || height <= 0) return null

        // Apple TV Remote mini-player expects a 1:1 square artwork aspect ratio.
        val squareSize = minOf(width, height)
        val xOffset = (width - squareSize) / 2
        val yOffset = (height - squareSize) / 2
        val cropped = Bitmap.createBitmap(bitmap, xOffset, yOffset, squareSize, squareSize)
        val scaled = if (squareSize != targetDim) {
            Bitmap.createScaledBitmap(cropped, targetDim, targetDim, true)
        } else {
            cropped
        }

        val stream = ByteArrayOutputStream()
        scaled.compress(Bitmap.CompressFormat.JPEG, 85, stream)
        if (scaled !== cropped) scaled.recycle()
        if (cropped !== bitmap) cropped.recycle()
        return stream.toByteArray()
    }

    private fun fetchArtworkFromUriAsync(uriString: String, artKey: String) {
        pendingArtUri = uriString
        artworkExecutor.execute {
            try {
                val bitmap: Bitmap? = if (uriString.startsWith("http://") || uriString.startsWith("https://")) {
                    val conn = URL(uriString).openConnection() as HttpURLConnection
                    conn.connectTimeout = 5000
                    conn.readTimeout = 5000
                    conn.doInput = true
                    conn.connect()
                    conn.inputStream.use { BitmapFactory.decodeStream(it) }
                } else if (uriString.startsWith("content://") || uriString.startsWith("file://")) {
                    appContext?.contentResolver?.openInputStream(Uri.parse(uriString))?.use {
                        BitmapFactory.decodeStream(it)
                    }
                } else {
                    null
                }
                if (bitmap != null) {
                    val bytes = compressSquareArtwork(bitmap)
                    bitmap.recycle()
                    if (bytes != null && bytes.isNotEmpty()) {
                        artworkUriCache.put(uriString, bytes)
                        cachedArtKey = artKey
                        cachedArtBytes = bytes
                        mainHandler.post { publishCurrentMedia(force = true) }
                    }
                }
            } catch (e: Exception) {
                Log.d(TAG, "Artwork download failed for $uriString: ${e.message}")
            } finally {
                if (pendingArtUri == uriString) {
                    pendingArtUri = null
                }
            }
        }
    }

    fun publishCurrentMedia(force: Boolean = false) {
        val controller = activeController
        val metadata = controller?.metadata
        val state = controller?.playbackState

        val title = metadata?.getString(MediaMetadata.METADATA_KEY_TITLE)
            ?: metadata?.description?.title?.toString()
            ?: if (controller != null) "Медиаплеер" else null
        val artist = metadata?.getString(MediaMetadata.METADATA_KEY_ARTIST)
            ?: metadata?.getString(MediaMetadata.METADATA_KEY_ALBUM_ARTIST)
            ?: metadata?.description?.subtitle?.toString()
            ?: if (controller != null) controller.packageName else null
        val album = metadata?.getString(MediaMetadata.METADATA_KEY_ALBUM)

        val durationMs = metadata?.getLong(MediaMetadata.METADATA_KEY_DURATION) ?: 0L
        val duration = durationMs.toDouble() / 1000.0
        val livePosMs = computeLivePositionMs(state)
        val clampedPosMs = if (durationMs > 0L) livePosMs.coerceAtMost(durationMs) else livePosMs
        val elapsed = clampedPosMs.toDouble() / 1000.0
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

        val artKey = "$title|$artist"
        val artUri = try {
            metadata?.getString(MediaMetadata.METADATA_KEY_ART_URI)
                ?: metadata?.getString(MediaMetadata.METADATA_KEY_ALBUM_ART_URI)
                ?: metadata?.getString(MediaMetadata.METADATA_KEY_DISPLAY_ICON_URI)
                ?: metadata?.description?.iconUri?.toString()
        } catch (_: Exception) {
            null
        }

        val cachedFromUri = if (!artUri.isNullOrEmpty()) artworkUriCache.get(artUri) else null
        val hasCachedArt = (artKey == cachedArtKey && cachedArtBytes != null) || cachedFromUri != null

        val nowMs = System.currentTimeMillis()
        val dedupeKey = "$title|$artist|$album|$duration|$isPlaying|$rate|$hasCachedArt"
        if (!force && dedupeKey == lastPublishedKey && (nowMs - lastPublishedAtMs) < 500L) {
            return
        }
        lastPublishedKey = dedupeKey
        lastPublishedAtMs = nowMs

        val bitmap = if (hasCachedArt) {
            null
        } else {
            try {
                metadata?.getBitmap(MediaMetadata.METADATA_KEY_ART)
                    ?: metadata?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART)
                    ?: metadata?.getBitmap(MediaMetadata.METADATA_KEY_DISPLAY_ICON)
                    ?: metadata?.description?.iconBitmap
            } catch (_: Exception) {
                null
            }
        }

        if (bitmap == null && !hasCachedArt && !artUri.isNullOrEmpty() && pendingArtUri != artUri) {
            fetchArtworkFromUriAsync(artUri, artKey)
        }

        mediaExecutor.execute {
            var artBytes: ByteArray? = cachedFromUri ?: if (artKey == cachedArtKey) cachedArtBytes else null
            if (bitmap != null && !bitmap.isRecycled) {
                try {
                    val compressed = compressSquareArtwork(bitmap)
                    if (compressed != null && compressed.isNotEmpty()) {
                        artBytes = compressed
                        cachedArtKey = artKey
                        cachedArtBytes = artBytes
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "Failed to compress artwork: ${e.message}")
                }
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
                Log.d(TAG, "Published media: title=$title artist=$artist isPlaying=$isPlaying elapsed=${elapsed.toInt()}s art=${artBytes?.size ?: 0}B")
            } catch (e: Exception) {
                Log.w(TAG, "Failed to call nativeUpdateMediaInfo: ${e.message}")
            }
        }
    }

    fun handleMediaCommand(command: String, param: Long) {
        val lower = command.lowercase().trim()
        Log.i(TAG, "handleMediaCommand: $lower (param=$param)")

        val controller = activeController
        val controls = controller?.transportControls

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
            "toggle", "play_pause" -> {
                val isPlaying = controller?.playbackState?.state == PlaybackState.STATE_PLAYING
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
            "skip" -> {
                val deltaSec = if (param == 0L) 10L else param
                if (controls != null) {
                    val currentMs = computeLivePositionMs(controller.playbackState)
                    val durationMs = controller.metadata?.getLong(MediaMetadata.METADATA_KEY_DURATION) ?: 0L
                    val maxMs = if (durationMs > 0L) durationMs else Long.MAX_VALUE
                    val targetMs = (currentMs + deltaSec * 1000L).coerceIn(0L, maxMs)
                    Log.i(TAG, "Skipping by ${deltaSec}s: ${currentMs}ms -> ${targetMs}ms")
                    controls.seekTo(targetMs)
                    mainHandler.postDelayed({ publishCurrentMedia(force = true) }, 300L)
                } else {
                    LocalAdbDriver.sendKey(if (deltaSec >= 0) "media_fast_forward" else "media_rewind")
                }
            }
            "seek" -> {
                if (controls != null) {
                    controls.seekTo((param * 1000L).coerceAtLeast(0L))
                    mainHandler.postDelayed({ publishCurrentMedia(force = true) }, 300L)
                }
            }
            else -> {
                Log.w(TAG, "Unknown media command: $command")
            }
        }
    }
}
