package com.carriez.flutter_hbb

import android.content.Context
import android.util.Log
import androidx.annotation.Keep
import org.json.JSONObject
import ffi.FFI

/**
 * 示例类，演示如何使用RustDesk的FFI事件回调功能
 * 这个类展示了如何注册全局事件回调和会话事件回调
 */
class EventCallbackExample(private val context: Context) {
    private val logTag = "EventCallbackExample"
    
    // 应用类型标识，用于区分不同的回调注册
    private val appType = "android_example"
    
    /**
     * 初始化并注册全局事件回调
     */
    fun initialize() {
        // 初始化FFI
        FFI.init(context)
        
        // 注册全局事件回调
        val registered = FFI.registerGlobalEventCallback(appType, GlobalEventCallback())
        Log.d(logTag, "Global event callback registered: $registered")
    }
    
    /**
     * 注销全局事件回调
     */
    fun cleanup() {
        val unregistered = FFI.unregisterGlobalEventCallback(appType)
        Log.d(logTag, "Global event callback unregistered: $unregistered")
    }
    
    /**
     * 为特定会话注册事件回调
     * @param sessionId 会话ID
     * @return 是否注册成功
     */
    fun registerSessionCallback(sessionId: String): Boolean {
        val registered = FFI.registerSessionEventCallback(sessionId, SessionEventCallback())
        Log.d(logTag, "Session event callback registered for session $sessionId: $registered")
        return registered
    }
    
    /**
     * 启动一个新会话并注册回调
     * @param sessionId 会话ID
     * @param peerId 对端ID
     * @return 是否启动成功
     */
    fun startNewSession(sessionId: String, peerId: String): Boolean {
        val started = FFI.startSession(sessionId, peerId, SessionEventCallback())
        Log.d(logTag, "Session started with ID $sessionId for peer $peerId: $started")
        return started
    }
    
    /**
     * 全局事件回调实现类
     * 用于接收来自Rust的全局事件通知
     */
    @Keep
    inner class GlobalEventCallback {
        /**
         * 处理来自Rust的事件回调
         * @param event 事件名称
         * @param jsonData 事件数据（JSON格式）
         */
        @Keep
        fun onEvent(event: String, jsonData: String) {
            Log.d(logTag, "Global event received: $event, data: $jsonData")
            try {
                val json = JSONObject(jsonData)
                when (event) {
                    "connection" -> {
                        // 处理连接事件
                        val id = json.optInt("id")
                        val peerId = json.optString("peer_id")
                        val connected = json.optBoolean("connected")
                        Log.d(logTag, "Connection event: id=$id, peerId=$peerId, connected=$connected")
                        
                        // 这里可以添加UI更新或其他业务逻辑
                    }
                    "permission" -> {
                        // 处理权限事件
                        val name = json.optString("name")
                        val granted = json.optBoolean("granted")
                        Log.d(logTag, "Permission event: $name granted=$granted")
                    }
                    "config_updated" -> {
                        // 配置更新事件
                        Log.d(logTag, "Config updated")
                    }
                    else -> {
                        Log.d(logTag, "Unknown global event: $event")
                    }
                }
            } catch (e: Exception) {
                Log.e(logTag, "Error parsing event data: ${e.message}")
            }
        }
    }
    
    /**
     * 会话事件回调实现类
     * 用于接收特定会话的事件通知
     */
    @Keep
    inner class SessionEventCallback {
        /**
         * 处理来自Rust的会话事件回调
         * @param event 事件名称
         * @param jsonData 事件数据（JSON格式）
         */
        @Keep
        fun onEvent(event: String, jsonData: String) {
            Log.d(logTag, "Session event received: $event, data: $jsonData")
            try {
                val json = JSONObject(jsonData)
                when (event) {
                    "video_frame" -> {
                        // 处理视频帧事件
                        val width = json.optInt("width")
                        val height = json.optInt("height")
                        Log.d(logTag, "Video frame: $width x $height")
                    }
                    "audio_frame" -> {
                        // 处理音频帧事件
                        val sampleRate = json.optInt("sample_rate")
                        Log.d(logTag, "Audio frame: sample rate $sampleRate")
                    }
                    "close" -> {
                        // 会话关闭事件
                        val reason = json.optString("reason")
                        Log.d(logTag, "Session closed: $reason")
                    }
                    else -> {
                        Log.d(logTag, "Unknown session event: $event")
                    }
                }
            } catch (e: Exception) {
                Log.e(logTag, "Error parsing session event data: ${e.message}")
            }
        }
    }
    
    /**
     * 使用示例
     */
    companion object {
        /**
         * 在Activity或Service中使用的示例代码
         */
        fun usageExample(context: Context) {
            // 创建实例
            val example = EventCallbackExample(context)
            
            // 初始化并注册全局回调
            example.initialize()
            
            // 注册特定会话的回调
            val sessionId = "session_123"
            example.registerSessionCallback(sessionId)
            
            // 启动新会话
            val peerId = "peer_456"
            example.startNewSession(sessionId, peerId)
            
            // 在应用退出时清理资源
            // example.cleanup()
        }
    }
}