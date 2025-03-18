// ffi.kt

package ffi

import android.content.Context
import java.nio.ByteBuffer

import com.carriez.flutter_hbb.RdClipboardManager

// 添加事件回调接口
interface EventCallback {
    fun onEvent(event: String)
}

object FFI {
    init {
        System.loadLibrary("rustdesk")
    }

    external fun init(ctx: Context)
    external fun setClipboardManager(clipboardManager: RdClipboardManager)
    external fun startServer(app_dir: String, custom_client_config: String)
    external fun startService()
    external fun onVideoFrameUpdate(buf: ByteBuffer)
    external fun onAudioFrameUpdate(buf: ByteBuffer)
    external fun translateLocale(localeName: String, input: String): String
    external fun refreshScreen()
    external fun setFrameRawEnable(name: String, value: Boolean)
    external fun setCodecInfo(info: String)
    external fun getLocalOption(key: String): String
    external fun onClipboardUpdate(clips: ByteBuffer)
    external fun isServiceClipboardEnabled(): Boolean
    external fun getMyId(): String
    
    // 添加全局事件流相关方法
    external fun startGlobalEventStream(callback: EventCallback, appType: String): Boolean
    external fun stopGlobalEventStream(appType: String)
    external fun pushGlobalEvent(channel: String, event: String): Boolean
    external fun getGlobalEventChannels(): String  // 修改返回类型为String，因为Rust端返回JSON字符串
    
    // 添加事件流相关方法
    external fun addEventStream(appType: String, callback: EventCallback)
    external fun pushEvent(appType: String, event: String)
    external fun clearEventStream(appType: String)
    external fun setCurrentSessionId(sessionId: String)
    external fun getCurrentSessionId(): String
    
    // 添加会话管理相关方法
    external fun sessionAdd(
        sessionId: String,
        id: String,
        isFileTransfer: Boolean,
        isPortForward: Boolean,
        isRdp: Boolean,
        switchUuid: String,
        forceRelay: Boolean,
        password: String,
        isSharedPassword: Boolean
    ): String
    
    external fun sessionClose(sessionId: String)
    external fun sessionRefresh(sessionId: String, display: Int)
    external
    // 继续添加会话管理相关方法
    external fun sessionInputKey(
        sessionId: String,
        name: String,
        down: Boolean,
        press: Boolean,
        alt: Boolean,
        ctrl: Boolean,
        shift: Boolean,
        command: Boolean
    )
    
    external fun sessionInputString(sessionId: String, value: String)
    external fun sessionLockScreen(sessionId: String)
    external fun sessionCtrlAltDel(sessionId: String)
    external fun sessionSwitchDisplay(isDesktop: Boolean, sessionId: String, value: IntArray)
    external fun sessionReconnect(sessionId: String, forceRelay: Boolean)
    
    // 文件传输相关方法
    external fun sessionReadRemoteDir(sessionId: String, path: String, includeHidden: Boolean)
    external fun sessionSendFiles(
        sessionId: String,
        actId: Int,
        path: String,
        to: String,
        fileNum: Int,
        includeHidden: Boolean,
        isRemote: Boolean,
        isDir: Boolean
    )
    external fun sessionCancelJob(sessionId: String, actId: Int)
    external fun sessionRemoveFile(
        sessionId: String,
        actId: Int,
        path: String,
        fileNum: Int,
        isRemote: Boolean
    )
    external fun sessionCreateDir(sessionId: String, actId: Int, path: String, isRemote: Boolean)
    
    // 其他控制方法
    external fun sessionGetPlatform(sessionId: String, isRemote: Boolean): String
    external fun sessionGetToggleOption(sessionId: String, arg: String): Boolean
    external fun sessionToggleOption(sessionId: String, value: String)
    external fun sessionGetImageQuality(sessionId: String): String?
    external fun sessionSetImageQuality(sessionId: String, value: String)
    
    // 系统信息和配置方法
    external fun getMyId(): String
    external fun getUuid(): String
    external fun getVersion(): String
    external fun getOption(key: String): String
    external fun setOption(key: String, value: String)
    external fun getOptions(): String
    external fun setOptions(json: String)
    external fun getPeerOption(id: String, key: String): String
    external fun setPeerOption(id: String, key: String, value: String): Boolean
    external fun getFav(): Array<String>
    external fun storeFav(favs: Array<String>)
    external fun getPeers(): String
    external fun getRecentPeers(): String
    external fun getLanPeers(): String
    external fun removePeer(id: String)
    
    // 系统操作
    external fun changeTheme(dark: String)
    external fun changeLanguage(lang: String)
    external fun videoSaveDirectory(root: Boolean): String
    external fun getMainDisplay(): String
    external fun getDisplays(): String
}
