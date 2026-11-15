package com.corvofeng.fakeatv

import android.content.ComponentName
import android.service.notification.NotificationListenerService
import android.util.Log

class AtvNotificationListenerService : NotificationListenerService() {
    companion object {
        private const val TAG = "AtvNotificationListener"
        var instance: AtvNotificationListenerService? = null
            private set
    }

    override fun onListenerConnected() {
        super.onListenerConnected()
        instance = this
        Log.i(TAG, "AtvNotificationListenerService connected")
        AtvMediaManager.init(this, ComponentName(this, AtvNotificationListenerService::class.java))
    }

    override fun onListenerDisconnected() {
        super.onListenerDisconnected()
        instance = null
        Log.i(TAG, "AtvNotificationListenerService disconnected")
    }
}
