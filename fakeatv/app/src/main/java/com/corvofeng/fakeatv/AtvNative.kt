package com.corvofeng.fakeatv

object AtvNative {
    init {
        try {
            System.loadLibrary("atv_android")
        } catch (e: UnsatisfiedLinkError) {
            e.printStackTrace()
        }
    }

    interface Callback {
        fun onButton(name: String)
        fun onTouch(dx: Double, dy: Double, phase: Int)
        fun onAudio(volume: Double, muted: Boolean)
        fun onEvent(kind: String, detail: String)
        fun onTextInput(text: String)
        fun onMediaCommand(command: String, param: Long)
    }

    external fun nativeStartServer(
        name: String,
        pin: Int,
        deviceId: String,
        serverId: String,
        localIp: String,
        callback: Callback
    ): Boolean

    external fun nativeStopServer()
    external fun nativeIsRunning(): Boolean

    external fun nativeUpdateMediaInfo(
        title: String?,
        artist: String?,
        album: String?,
        appName: String?,
        appBundleId: String?,
        duration: Double,
        elapsed: Double,
        rate: Float,
        isPlaying: Boolean,
        artworkData: ByteArray?
    )

    external fun nativeGetAirplayPort(): Int
    external fun nativeGetCompanionPort(): Int
    external fun nativeReannounceMdns()
    external fun nativeSetKeyboardFocus(focused: Boolean)
}
