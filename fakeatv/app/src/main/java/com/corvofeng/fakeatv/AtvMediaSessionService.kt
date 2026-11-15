package com.corvofeng.fakeatv

import android.content.ComponentName
import android.service.notification.NotificationListenerService
import android.util.Log

class AtvMediaSessionService : NotificationListenerService() {
    companion object {
        private const val TAG = "AtvMediaSessionService"
        var instance: AtvMediaSessionService? = null
            private set
    }

    override fun onListenerConnected() {
        super.onListenerConnected()
        instance = this
        Log.i(TAG, "AtvMediaSessionService connected as NotificationListener")
        AtvMediaManager.init(this, ComponentName(this, AtvMediaSessionService::class.java))
    }

    override fun onListenerDisconnected() {
        super.onListenerDisconnected()
        instance = null
        Log.i(TAG, "AtvMediaSessionService disconnected")
    }
}
