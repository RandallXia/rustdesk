package com.carriez.flutter_hbb

import ffi.EventCallback
import ffi.FFI

class DeskEvent : EventCallback {
    
    fun startListening() {
        FFI.startGlobalEventStream(this, "main")
    }
    
    fun stopListening() {
        FFI.stopGlobalEventStream("main")
    }
    
    override fun onEvent(event: String) {
        // 处理接收到的事件
        println("Received event: $event")
    }
    
    fun sendEvent(channel: String, event: String) {
        FFI.pushGlobalEvent(channel, event)
    }
    
    fun getAllChannels(): Array<String> {
        return FFI.getGlobalEventChannels()
    }
}