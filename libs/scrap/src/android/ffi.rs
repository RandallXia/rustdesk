use jni::objects::JByteBuffer;
use jni::objects::JString;
use jni::objects::JValue;
use jni::sys::jboolean;
use jni::sys::jstring;
use jni::JNIEnv;
use jni::{
    objects::{GlobalRef, JClass, JObject},
    strings::JNIString,
    JavaVM,
};

use rustdesk::{
    common::{make_fd_to_json, make_vec_fd_to_json},
    flutter::{
        self, session_add, session_add_existed, session_start_, sessions, try_sync_peer_option,
    },
    input::*,
    ui_interface::{self, *},
    client::*,
    flutter_ffi::{EventToUI, SessionID},
    ui_session_interface::{io_loop, InvokeUiSession, Session},
};
use serde_json::{json, Value};
use hbb_common::{
    config::{self, LocalConfig, PeerConfig, PeerInfoSerde},
    fs, log,
    message_proto::FileDirectory, // 正确导入 FileDirectory
    rendezvous_proto::ConnType,
    ResultType, // 只保留一次导入
    protobuf::Message,
    message_proto::MultiClipboards,
};
use jni::errors::{Error as JniError, Result as JniResult};
use lazy_static::lazy_static;
use serde::Deserialize;
use std::collections::HashMap;
use std::ops::Not;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicPtr, Ordering::SeqCst};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

// 定义 SessionID 类型
pub type SessionID = uuid::Uuid;

lazy_static! {
    static ref JVM: RwLock<Option<JavaVM>> = RwLock::new(None);
    static ref MAIN_SERVICE_CTX: RwLock<Option<GlobalRef>> = RwLock::new(None); // MainService -> video service / audio service / info
    static ref VIDEO_RAW: Mutex<FrameRaw> = Mutex::new(FrameRaw::new("video", MAX_VIDEO_FRAME_TIMEOUT));
    static ref AUDIO_RAW: Mutex<FrameRaw> = Mutex::new(FrameRaw::new("audio", MAX_AUDIO_FRAME_TIMEOUT));
    static ref NDK_CONTEXT_INITED: Mutex<bool> = Default::default();
    static ref MEDIA_CODEC_INFOS: RwLock<Option<MediaCodecInfos>> = RwLock::new(None);
    static ref CLIPBOARD_MANAGER: RwLock<Option<GlobalRef>> = RwLock::new(None);
    static ref CLIPBOARDS_HOST: Mutex<Option<MultiClipboards>> = Mutex::new(None);
    static ref CLIPBOARDS_CLIENT: Mutex<Option<MultiClipboards>> = Mutex::new(None);
    // 添加全局事件流的存储
    static ref GLOBAL_EVENT_CALLBACKS: RwLock<HashMap<String, GlobalRef>> = RwLock::new(HashMap::new());
    // 添加会话管理相关的存储
    static ref SESSIONS: RwLock<HashMap<SessionID, Arc<Session>>> = RwLock::new(HashMap::new());
}

const MAX_VIDEO_FRAME_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_AUDIO_FRAME_TIMEOUT: Duration = Duration::from_millis(1000);

struct FrameRaw {
    name: &'static str,
    ptr: AtomicPtr<u8>,
    len: usize,
    last_update: Instant,
    timeout: Duration,
    enable: bool,
}

impl FrameRaw {
    fn new(name: &'static str, timeout: Duration) -> Self {
        FrameRaw {
            name,
            ptr: AtomicPtr::default(),
            len: 0,
            last_update: Instant::now(),
            timeout,
            enable: false,
        }
    }

    fn set_enable(&mut self, value: bool) {
        self.enable = value;
        self.ptr.store(std::ptr::null_mut(), SeqCst);
        self.len = 0;
    }

    fn update(&mut self, data: *mut u8, len: usize) {
        if self.enable.not() {
            return;
        }
        self.len = len;
        self.ptr.store(data, SeqCst);
        self.last_update = Instant::now();
    }

    // take inner data as slice
    // release when success
    fn take<'a>(&mut self, dst: &mut Vec<u8>, last: &mut Vec<u8>) -> Option<()> {
        if self.enable.not() {
            return None;
        }
        let ptr = self.ptr.load(SeqCst);
        if ptr.is_null() || self.len == 0 {
            None
        } else {
            if self.last_update.elapsed() > self.timeout {
                log::trace!("Failed to take {} raw,timeout!", self.name);
                return None;
            }
            let slice = unsafe { std::slice::from_raw_parts(ptr, self.len) };
            self.release();
            if last.len() == slice.len() && crate::would_block_if_equal(last, slice).is_err() {
                return None;
            }
            dst.resize(slice.len(), 0);
            unsafe {
                std::ptr::copy_nonoverlapping(slice.as_ptr(), dst.as_mut_ptr(), slice.len());
            }
            Some(())
        }
    }

    fn release(&mut self) {
        self.len = 0;
        self.ptr.store(std::ptr::null_mut(), SeqCst);
    }
}

pub fn get_video_raw<'a>(dst: &mut Vec<u8>, last: &mut Vec<u8>) -> Option<()> {
    VIDEO_RAW.lock().ok()?.take(dst, last)
}

pub fn get_audio_raw<'a>(dst: &mut Vec<u8>, last: &mut Vec<u8>) -> Option<()> {
    AUDIO_RAW.lock().ok()?.take(dst, last)
}

pub fn get_clipboards(client: bool) -> Option<MultiClipboards> {
    if client {
        CLIPBOARDS_CLIENT.lock().ok()?.take()
    } else {
        CLIPBOARDS_HOST.lock().ok()?.take()
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getMyId(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let id = hbb_common::config::Config::get_id();
    let output = env.new_string(id).expect("Failed to create Java string");
    output.into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_onVideoFrameUpdate(
    env: JNIEnv,
    _class: JClass,
    buffer: JObject,
) {
    let jb = JByteBuffer::from(buffer);
    if let Ok(data) = env.get_direct_buffer_address(&jb) {
        if let Ok(len) = env.get_direct_buffer_capacity(&jb) {
            VIDEO_RAW.lock().unwrap().update(data, len);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_onAudioFrameUpdate(
    env: JNIEnv,
    _class: JClass,
    buffer: JObject,
) {
    let jb = JByteBuffer::from(buffer);
    if let Ok(data) = env.get_direct_buffer_address(&jb) {
        if let Ok(len) = env.get_direct_buffer_capacity(&jb) {
            AUDIO_RAW.lock().unwrap().update(data, len);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_onClipboardUpdate(
    env: JNIEnv,
    _class: JClass,
    buffer: JByteBuffer,
) {
    if let Ok(data) = env.get_direct_buffer_address(&buffer) {
        if let Ok(len) = env.get_direct_buffer_capacity(&buffer) {
            let data = unsafe { std::slice::from_raw_parts(data, len) };
            if let Ok(clips) = MultiClipboards::parse_from_bytes(&data[1..]) {
                let is_client = data[0] == 1;
                if is_client {
                    *CLIPBOARDS_CLIENT.lock().unwrap() = Some(clips);
                } else {
                    *CLIPBOARDS_HOST.lock().unwrap() = Some(clips);
                }
            }
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setFrameRawEnable(
    env: JNIEnv,
    _class: JClass,
    name: JString,
    value: jboolean,
) {
    let mut env = env;
    if let Ok(name) = env.get_string(&name) {
        let name: String = name.into();
        let value = value.eq(&1);
        if name.eq("video") {
            VIDEO_RAW.lock().unwrap().set_enable(value);
        } else if name.eq("audio") {
            AUDIO_RAW.lock().unwrap().set_enable(value);
        }
    };
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_init(env: JNIEnv, _class: JClass, ctx: JObject) {
    log::debug!("MainService init from java");
    if let Ok(jvm) = env.get_java_vm() {
        let java_vm = jvm.get_java_vm_pointer() as *mut c_void;
        let mut jvm_lock = JVM.write().unwrap();
        if jvm_lock.is_none() {
            *jvm_lock = Some(jvm);
        }
        drop(jvm_lock);
        if let Ok(context) = env.new_global_ref(ctx) {
            let context_jobject = context.as_obj().as_raw() as *mut c_void;
            *MAIN_SERVICE_CTX.write().unwrap() = Some(context);
            init_ndk_context(java_vm, context_jobject);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setClipboardManager(
    env: JNIEnv,
    _class: JClass,
    clipboard_manager: JObject,
) {
    log::debug!("ClipboardManager init from java");
    if let Ok(jvm) = env.get_java_vm() {
        let java_vm = jvm.get_java_vm_pointer() as *mut c_void;
        let mut jvm_lock = JVM.write().unwrap();
        if jvm_lock.is_none() {
            *jvm_lock = Some(jvm);
        }
        drop(jvm_lock);
        if let Ok(manager) = env.new_global_ref(clipboard_manager) {
            *CLIPBOARD_MANAGER.write().unwrap() = Some(manager);
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct MediaCodecInfo {
    pub name: String,
    pub is_encoder: bool,
    #[serde(default)]
    pub hw: Option<bool>, // api 29+
    pub mime_type: String,
    pub surface: bool,
    pub nv12: bool,
    #[serde(default)]
    pub low_latency: Option<bool>, // api 30+, decoder
    pub min_bitrate: u32,
    pub max_bitrate: u32,
    pub min_width: usize,
    pub max_width: usize,
    pub min_height: usize,
    pub max_height: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MediaCodecInfos {
    pub version: usize,
    pub w: usize, // aligned
    pub h: usize, // aligned
    pub codecs: Vec<MediaCodecInfo>,
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setCodecInfo(env: JNIEnv, _class: JClass, info: JString) {
    let mut env = env;
    if let Ok(info) = env.get_string(&info) {
        let info: String = info.into();
        if let Ok(infos) = serde_json::from_str::<MediaCodecInfos>(&info) {
            *MEDIA_CODEC_INFOS.write().unwrap() = Some(infos);
        }
    }
}

pub fn get_codec_info() -> Option<MediaCodecInfos> {
    MEDIA_CODEC_INFOS.read().unwrap().as_ref().cloned()
}

pub fn clear_codec_info() {
    *MEDIA_CODEC_INFOS.write().unwrap() = None;
}

// another way to fix "reference table overflow" error caused by new_string and call_main_service_pointer_input frequently calld
// is below, but here I change kind from string to int for performance
/*
        env.with_local_frame(10, || {
            let kind = env.new_string(kind)?;
            env.call_method(
                ctx,
                "rustPointerInput",
                "(Ljava/lang/String;III)V",
                &[
                    JValue::Object(&JObject::from(kind)),
                    JValue::Int(mask),
                    JValue::Int(x),
                    JValue::Int(y),
                ],
            )?;
            Ok(JObject::null())
        })?;
*/
pub fn call_main_service_pointer_input(kind: &str, mask: i32, x: i32, y: i32) -> JniResult<()> {
    if let (Some(jvm), Some(ctx)) = (
        JVM.read().unwrap().as_ref(),
        MAIN_SERVICE_CTX.read().unwrap().as_ref(),
    ) {
        let mut env = jvm.attach_current_thread_as_daemon()?;
        let kind = if kind == "touch" { 0 } else { 1 };
        env.call_method(
            ctx,
            "rustPointerInput",
            "(IIII)V",
            &[
                JValue::Int(kind),
                JValue::Int(mask),
                JValue::Int(x),
                JValue::Int(y),
            ],
        )?;
        return Ok(());
    } else {
        return Err(JniError::ThrowFailed(-1));
    }
}

pub fn call_main_service_key_event(data: &[u8]) -> JniResult<()> {
    if let (Some(jvm), Some(ctx)) = (
        JVM.read().unwrap().as_ref(),
        MAIN_SERVICE_CTX.read().unwrap().as_ref(),
    ) {
        let mut env = jvm.attach_current_thread_as_daemon()?;
        let data = env.byte_array_from_slice(data)?;

        env.call_method(
            ctx,
            "rustKeyEventInput",
            "([B)V",
            &[JValue::Object(&JObject::from(data))],
        )?;
        return Ok(());
    } else {
        return Err(JniError::ThrowFailed(-1));
    }
}

fn _call_clipboard_manager<S, T>(name: S, sig: T, args: &[JValue]) -> JniResult<()>
where
    S: Into<JNIString>,
    T: Into<JNIString> + AsRef<str>,
{
    if let (Some(jvm), Some(cm)) = (
        JVM.read().unwrap().as_ref(),
        CLIPBOARD_MANAGER.read().unwrap().as_ref(),
    ) {
        let mut env = jvm.attach_current_thread()?;
        env.call_method(cm, name, sig, args)?;
        return Ok(());
    } else {
        return Err(JniError::ThrowFailed(-1));
    }
}

pub fn call_clipboard_manager_update_clipboard(data: &[u8]) -> JniResult<()> {
    if let (Some(jvm), Some(cm)) = (
        JVM.read().unwrap().as_ref(),
        CLIPBOARD_MANAGER.read().unwrap().as_ref(),
    ) {
        let mut env = jvm.attach_current_thread()?;
        let data = env.byte_array_from_slice(data)?;

        env.call_method(
            cm,
            "rustUpdateClipboard",
            "([B)V",
            &[JValue::Object(&JObject::from(data))],
        )?;
        return Ok(());
    } else {
        return Err(JniError::ThrowFailed(-1));
    }
}

pub fn call_clipboard_manager_enable_client_clipboard(enable: bool) -> JniResult<()> {
    _call_clipboard_manager(
        "rustEnableClientClipboard",
        "(Z)V",
        &[JValue::Bool(jboolean::from(enable))],
    )
}

pub fn call_main_service_get_by_name(name: &str) -> JniResult<String> {
    if let (Some(jvm), Some(ctx)) = (
        JVM.read().unwrap().as_ref(),
        MAIN_SERVICE_CTX.read().unwrap().as_ref(),
    ) {
        let mut env = jvm.attach_current_thread_as_daemon()?;
        let res = env.with_local_frame(10, |env| -> JniResult<String> {
            let name = env.new_string(name)?;
            let res = env
                .call_method(
                    ctx,
                    "rustGetByName",
                    "(Ljava/lang/String;)Ljava/lang/String;",
                    &[JValue::Object(&JObject::from(name))],
                )?
                .l()?;
            let res = JString::from(res);
            let res = env.get_string(&res)?;
            let res = res.to_string_lossy().to_string();
            Ok(res)
        })?;
        Ok(res)
    } else {
        return Err(JniError::ThrowFailed(-1));
    }
}

pub fn call_main_service_set_by_name(
    name: &str,
    arg1: Option<&str>,
    arg2: Option<&str>,
) -> JniResult<()> {
    if let (Some(jvm), Some(ctx)) = (
        JVM.read().unwrap().as_ref(),
        MAIN_SERVICE_CTX.read().unwrap().as_ref(),
    ) {
        let mut env = jvm.attach_current_thread_as_daemon()?;
        env.with_local_frame(10, |env| -> JniResult<()> {
            let name = env.new_string(name)?;
            let arg1 = env.new_string(arg1.unwrap_or(""))?;
            let arg2 = env.new_string(arg2.unwrap_or(""))?;

            env.call_method(
                ctx,
                "rustSetByName",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(&JObject::from(name)),
                    JValue::Object(&JObject::from(arg1)),
                    JValue::Object(&JObject::from(arg2)),
                ],
            )?;
            Ok(())
        })?;
        return Ok(());
    } else {
        return Err(JniError::ThrowFailed(-1));
    }
}

// Difference between MainService, MainActivity, JNI_OnLoad:
//  jvm is the same, ctx is differen and ctx of JNI_OnLoad is null.
//  cpal: all three works
//  Service(GetByName, ...): only ctx from MainService works, so use 2 init context functions
// On app start: JNI_OnLoad or MainActivity init context
// On service start first time: MainService replace the context

fn init_ndk_context(java_vm: *mut c_void, context_jobject: *mut c_void) {
    let mut lock = NDK_CONTEXT_INITED.lock().unwrap();
    if *lock {
        unsafe {
            ndk_context::release_android_context();
        }
        *lock = false;
    }
    unsafe {
        ndk_context::initialize_android_context(java_vm, context_jobject);
        #[cfg(feature = "hwcodec")]
        hwcodec::android::ffmpeg_set_java_vm(java_vm);
    }
    *lock = true;
}

// https://cjycode.com/flutter_rust_bridge/guides/how-to/ndk-init
#[no_mangle]
pub extern "C" fn JNI_OnLoad(vm: jni::JavaVM, res: *mut std::os::raw::c_void) -> jni::sys::jint {
    if let Ok(env) = vm.get_env() {
        let vm = vm.get_java_vm_pointer() as *mut std::os::raw::c_void;
        init_ndk_context(vm, res);
    }
    jni::JNIVersion::V6.into()
}

// 添加 Session 结构体
struct Session {
    id: SessionID,
    peer_id: String,
    // 其他会话相关字段
}

impl Session {
    fn new(id: SessionID, peer_id: String) -> Self {
        Self { id, peer_id }
    }
    
    // 会话相关方法
    fn close(&self) {
        // 关闭会话的实现
    }
    
    fn refresh_video(&self, display: i32) {
        // 刷新视频的实现
    }
    
    fn input_key(&self, name: &str, down: bool, press: bool, alt: bool, ctrl: bool, shift: bool, command: bool) {
        // 处理键盘输入的实现
    }
    
    fn input_string(&self, value: &str) {
        // 处理字符串输入的实现
    }
    
    fn lock_screen(&self) {
        // 锁定屏幕的实现
    }
    
    fn ctrl_alt_del(&self) {
        // 发送 Ctrl+Alt+Del 的实现
    }
}

// 添加 Arc 和会话管理相关的导入
use std::sync::Arc;

// 初始化函数
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_initialize(
    env: JNIEnv,
    _class: JClass,
    app_dir: JString,
    custom_client_config: JString,
) {
    let app_dir: String = match env.get_string(app_dir) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_dir string: {:?}", e);
            return;
        }
    };
    
    let custom_client_config: String = match env.get_string(custom_client_config) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get custom_client_config string: {:?}", e);
            return;
        }
    };
    
    *config::APP_DIR.write().unwrap() = app_dir.to_owned();
    
    // 加载配置
    if custom_client_config.is_empty() {
        crate::load_custom_client();
    } else {
        crate::read_custom_client(&custom_client_config);
    }
    
    // Android 特定初始化
    #[cfg(debug_assertions)]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("ffi"),
    );
    #[cfg(not(debug_assertions))]
    hbb_common::init_log(false, "");
    
    #[cfg(feature = "mediacodec")]
    scrap::mediacodec::check_mediacodec();
    
    crate::common::test_rendezvous_server();
    crate::common::test_nat_type();
}

// 会话管理相关函数
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionAdd(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    id: JString,
    is_file_transfer: jboolean,
    is_port_forward: jboolean,
    is_rdp: jboolean,
    switch_uuid: JString,
    force_relay: jboolean,
    password: JString,
    is_shared_password: jboolean,
) -> jstring {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return env.new_string("Failed to get session_id").unwrap().into_raw();
        }
    };
    
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return env.new_string("Failed to get id").unwrap().into_raw();
        }
    };
    
    let switch_uuid: String = match env.get_string(switch_uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get switch_uuid string: {:?}", e);
            return env.new_string("Failed to get switch_uuid").unwrap().into_raw();
        }
    };
    
    let password: String = match env.get_string(password) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get password string: {:?}", e);
            return env.new_string("Failed to get password").unwrap().into_raw();
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return env.new_string("Failed to parse session_id as UUID").unwrap().into_raw();
        }
    };
    
    // 创建会话
    let session = Arc::new(Session::new(session_id, id.clone()));
    
    // 添加会话到管理器
    match SESSIONS.write() {
        Ok(mut sessions) => {
            sessions.insert(session_id, session);
        }
        Err(e) => {
            log::error!("Failed to add session: {:?}", e);
            return env.new_string(format!("Failed to add session with id {}, {}", &id, e)).unwrap().into_raw();
        }
    }
    
    env.new_string("").unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSwitchDisplay(
    env: JNIEnv,
    _class: JClass,
    is_desktop: jboolean,
    session_id: JString,
    value_array: jobject,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 将 Java 整数数组转换为 Rust Vec<i32>
    let value = match env.get_int_array_elements(value_array as jintArray, JNI_FALSE) {
        Ok((elements, _)) => {
            let len = env.get_array_length(value_array as jintArray).unwrap_or(0) as usize;
            let mut vec = Vec::with_capacity(len);
            for i in 0..len {
                vec.push(elements[i]);
            }
            vec
        }
        Err(e) => {
            log::error!("Failed to get int array elements: {:?}", e);
            return;
        }
    };
    
    // 切换显示器
    sessions::session_switch_display(is_desktop != 0, session_id, value);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setOption(
    env: JNIEnv,
    _class: JClass,
    key: JString,
    value: JString,
) {
    let key: String = match env.get_string(key) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get key string: {:?}", e);
            return;
        }
    };
    
    let value: String = match env.get_string(value) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get value string: {:?}", e);
            return;
        }
    };
    
    set_option(key, value);
}

// 辅助函数
fn get_uuid() -> String {
    hbb_common::config::Config::get_uuid()
}

fn get_option(key: String) -> String {
    ui_interface::get_option(key)
}

fn set_option(key: String, value: String) {
    ui_interface::set_option(key, value);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionGetPlatform(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    is_remote: jboolean,
) -> jstring {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    // 获取会话
    let session = match SESSIONS.read() {
        Ok(sessions) => sessions.get(&session_id).cloned(),
        Err(e) => {
            log::error!("Failed to get session: {:?}", e);
            None
        }
    };
    
    // 获取平台信息
    let platform = if let Some(session) = session {
        session.get_platform(is_remote != 0)
    } else {
        "".to_string()
    };
    
    env.new_string(platform).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionGetImageQuality(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) -> jstring {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    // 获取会话
    let session = match SESSIONS.read() {
        Ok(sessions) => sessions.get(&session_id).cloned(),
        Err(e) => {
            log::error!("Failed to get session: {:?}", e);
            None
        }
    };
    
    // 获取图像质量
    let quality = if let Some(session) = session {
        session.get_image_quality()
    } else {
        "".to_string()
    };
    
    env.new_string(quality).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSetImageQuality(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    value: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let value: String = match env.get_string(value) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get value string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 获取会话
    let session = match SESSIONS.read() {
        Ok(sessions) => sessions.get(&session_id).cloned(),
        Err(e) => {
            log::error!("Failed to get session: {:?}", e);
            None
        }
    };
    
    // 设置图像质量
    if let Some(session) = session {
        session.save_image_quality(value);
    }
}

// 对等点管理相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getPeers(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let peers = get_peers();
    env.new_string(peers).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getRecentPeers(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let peers = get_recent_peers();
    env.new_string(peers).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getLanPeers(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let peers = get_lan_peers();
    env.new_string(peers).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_removePeer(
    env: JNIEnv,
    _class: JClass,
    id: JString,
) {
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return;
        }
    };
    
    remove_peer(id);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getPeerOption(
    env: JNIEnv,
    _class: JClass,
    id: JString,
    key: JString,
) -> jstring {
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let key: String = match env.get_string(key) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get key string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let value = get_peer_option(id, key);
    env.new_string(value).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setPeerOption(
    env: JNIEnv,
    _class: JClass,
    id: JString,
    key: JString,
    value: JString,
) -> jboolean {
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return 0;
        }
    };
    
    let key: String = match env.get_string(key) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get key string: {:?}", e);
            return 0;
        }
    };
    
    let value: String = match env.get_string(value) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get value string: {:?}", e);
            return 0;
        }
    };
    
    set_peer_option(id, key, value);
    1
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getFav(
    env: JNIEnv,
    _class: JClass,
) -> jobjectArray {
    let favs = get_fav();
    let size = favs.len() as i32;
    
    let string_class = env.find_class("java/lang/String").unwrap();
    let result = env.new_object_array(size, string_class, JObject::null()).unwrap();
    
    for (i, fav) in favs.iter().enumerate() {
        let j_fav = env.new_string(fav).unwrap();
        env.set_object_array_element(result, i as i32, j_fav).unwrap();
    }
    
    result
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_storeFav(
    env: JNIEnv,
    _class: JClass,
    favs: jobjectArray,
) {
    let len = env.get_array_length(favs).unwrap_or(0);
    let mut vec_favs = Vec::with_capacity(len as usize);
    
    for i in 0..len {
        let obj = env.get_object_array_element(favs, i).unwrap();
        let s: String = env.get_string(obj.into()).unwrap().into();
        vec_favs.push(s);
    }
    
    store_fav(vec_favs);
}

// 系统操作相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_changeTheme(
    env: JNIEnv,
    _class: JClass,
    dark: JString,
) {
    let dark: String = match env.get_string(dark) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get dark string: {:?}", e);
            return;
        }
    };
    
    change_theme(dark);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_videoSaveDirectory(
    env: JNIEnv,
    _class: JClass,
    root: jboolean,
) -> jstring {
    let dir = video_save_directory(root != 0);
    env.new_string(dir).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getMainDisplay(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let display_info = get_main_display();
    env.new_string(display_info).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getDisplays(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let displays_info = get_displays();
    env.new_string(displays_info).unwrap().into_raw()
}

// 辅助函数
fn get_peers() -> String {
    ui_interface::get_peers()
}

fn get_recent_peers() -> String {
    ui_interface::get_recent_peers()
}

fn get_lan_peers() -> String {
    ui_interface::get_lan_peers()
}

fn remove_peer(id: String) {
    ui_interface::remove_peer(id);
}

fn get_peer_option(id: String, key: String) -> String {
    ui_interface::get_peer_option(id, key)
}

fn set_peer_option(id: String, key: String, value: String) {
    ui_interface::set_peer_option(id, key, value);
}

fn get_fav() -> Vec<String> {
    ui_interface::get_fav()
}

fn store_fav(favs: Vec<String>) {
    ui_interface::store_fav(favs);
}

fn change_theme(dark: String) {
    ui_interface::change_theme(dark);
}

fn change_language(lang: String) {
    ui_interface::change_language(lang);
}

fn video_save_directory(root: bool) -> String {
    ui_interface::video_save_directory(root)
}

fn get_main_display() -> String {
    #[cfg(not(target_os = "android"))]
    {
        "".to_string()
    }
    #[cfg(target_os = "android")]
    {
        let mut display_info = "".to_owned();
        if let Ok(displays) = crate::display_service::try_get_displays() {
            if let Some(display) = displays.iter().next() {
                display_info = serde_json::to_string(&HashMap::from([
                    ("w", display.width()),
                    ("h", display.height()),
                ]))
                .unwrap_or_default();
            }
        }
        display_info
    }
}

fn get_displays() -> String {
    #[cfg(not(target_os = "android"))]
    {
        "".to_string()
    }
    #[cfg(target_os = "android")]
    {
        let mut display_info = "".to_owned();
        if let Ok(displays) = crate::display_service::try_get_displays() {
            let displays = displays
                .iter()
                .map(|d| {
                    HashMap::from([
                        ("x", d.origin().0),
                        ("y", d.origin().1),
                        ("w", d.width() as i32),
                        ("h", d.height() as i32),
                    ])
                })
                .collect::<Vec<_>>();
            display_info = serde_json::to_string(&displays).unwrap_or_default();
        }
        display_info
    }
}

// 启动服务器
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_startServer(
    env: JNIEnv,
    _class: JClass,
    app_dir: JString,
    custom_client_config: JString,
) {
    let app_dir: String = match env.get_string(app_dir) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_dir string: {:?}", e);
            return;
        }
    };
    
    let custom_client_config: String = match env.get_string(custom_client_config) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get custom_client_config string: {:?}", e);
            return;
        }
    };
    
    // 启动服务器
    initialize(&app_dir, &custom_client_config);
}

// 推送全局事件
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_pushGlobalEvent(
    env: JNIEnv,
    _class: JClass,
    channel: JString,
    event: JString,
) -> jboolean {
    let channel: String = match env.get_string(channel) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get channel string: {:?}", e);
            return 0;
        }
    };
    
    let event: String = match env.get_string(event) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get event string: {:?}", e);
            return 0;
        }
    };
    
    // 推送事件
    match flutter::push_global_event(&channel, event) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

// 获取全局事件通道
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getGlobalEventChannels(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let channels = flutter::get_global_event_channels();
    let json = serde_json::to_string(&channels).unwrap_or_else(|_| "[]".to_string());
    env.new_string(json).unwrap().into_raw()
}

// 添加事件流相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_addEventStream(
    env: JNIEnv,
    _class: JClass,
    app_type: JString,
    event_stream: JObject,
) {
    let app_type: String = match env.get_string(app_type) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_type string: {:?}", e);
            return;
        }
    };
    
    let callback = match env.new_global_ref(event_stream) {
        Ok(global_ref) => global_ref,
        Err(e) => {
            log::error!("Failed to create global reference: {:?}", e);
            return;
        }
    };
    
    // 创建一个自定义的 StreamSink 实现
    let sink = AndroidEventSink::new(callback);
    
    // 添加事件流
    let _ = flutter::start_global_event_stream(Box::new(sink), app_type);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_pushEvent(
    env: JNIEnv,
    _class: JClass,
    app_type: JString,
    event: JString,
) {
    let app_type: String = match env.get_string(app_type) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_type string: {:?}", e);
            return;
        }
    };
    
    let event: String = match env.get_string(event) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get event string: {:?}", e);
            return;
        }
    };
    
    // 推送事件
    let _ = flutter::push_global_event(&app_type, event);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_clearEventStream(
    env: JNIEnv,
    _class: JClass,
    app_type: JString,
) {
    let app_type: String = match env.get_string(app_type) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_type string: {:?}", e);
            return;
        }
    };
    
    // 清除事件流
    flutter::stop_global_event_stream(app_type);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setCurrentSessionId(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    // 设置当前会话ID
    if let Ok(uuid) = uuid::Uuid::parse_str(&session_id) {
        *flutter::CUR_SESSION_ID.write().unwrap() = uuid;
    } else {
        log::error!("Failed to parse session_id as UUID");
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getCurrentSessionId(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let id = flutter::CUR_SESSION_ID.read().unwrap().to_string();
    env.new_string(id).unwrap().into_raw()
}

// 辅助函数
fn get_jvm() -> JavaVM {
    JVM.read().unwrap().clone().unwrap()
}

fn push_global_event(channel: &str, event: String) -> ResultType<()> {
    flutter::push_global_event(channel, event)
}

fn get_global_event_channels() -> Vec<String> {
    flutter::get_global_event_channels()
}

// 初始化函数
fn initialize(app_dir: &str, custom_client_config: &str) {
    *config::APP_DIR.write().unwrap() = app_dir.to_owned();
    
    // 加载自定义客户端配置
    if custom_client_config.is_empty() {
        crate::load_custom_client();
    } else {
        crate::read_custom_client(custom_client_config);
    }
    
    // 初始化日志
    #[cfg(debug_assertions)]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("ffi"),
    );
    #[cfg(not(debug_assertions))]
    hbb_common::init_log(false, "");
    
    // 检查媒体编解码器
    #[cfg(feature = "mediacodec")]
    scrap::mediacodec::check_mediacodec();
    
    // 测试服务器连接
    crate::common::test_rendezvous_server();
    crate::common::test_nat_type();
    
    // 启动异步任务运行器
    flutter::async_tasks::start_flutter_async_runner();
}

// 会话管理辅助函数
fn session_add(
    session_id: String,
    id: String,
    is_file_transfer: bool,
    is_port_forward: bool,
    is_rdp: bool,
    switch_uuid: String,
    force_relay: bool,
    password: String,
    is_shared_password: bool,
) -> ResultType<String> {
    let session_id = uuid::Uuid::parse_str(&session_id)?;
    flutter::session_add(
        &session_id,
        &id,
        is_file_transfer,
        is_port_forward,
        is_rdp,
        &switch_uuid,
        force_relay,
        password,
        is_shared_password,
        None,
    )
}

fn session_start(session_id: uuid::Uuid, id: String) -> ResultType<()> {
    // 创建一个自定义的 StreamSink 实现
    struct AndroidEventSink {
        session_id: uuid::Uuid,
    }
    
    impl StreamSink<String> for AndroidEventSink {
        fn add(&mut self, event: String) {
            let env = match get_jvm().attach_current_thread() {
                Ok(env) => env,
                Err(e) => {
                    log::error!("Failed to attach JVM thread: {:?}", e);
                    return;
                }
            };
            
            let event_jstring = match env.new_string(event) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to create Java string: {:?}", e);
                    return;
                }
            };
            
            let callback_obj = self.callback.as_obj();
            let _ = env.call_method(
                callback_obj,
                "onEvent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(event_jstring.into())],
            );
            
            if let Err(e) = env.exception_check() {
                log::error!("Exception occurred during callback: {:?}", e);
                let _ = env.exception_clear();
            }
        }
        
        fn close(&mut self) {
            // 关闭时的清理工作
        }
    }
    
    let sink = AndroidEventSink { session_id };
    flutter::session_start_(&session_id, &id, sink)
}

// 文件传输辅助函数
fn read_dir(path: &str, include_hidden: bool) -> ResultType<FileDirectory> {
    fs::read_dir(path, include_hidden)
}

fn get_platform(is_remote: bool) -> String {
    if is_remote {
        "".to_string() // 远程平台信息需要从会话中获取
    } else {
        #[cfg(target_os = "android")]
        return "Android".to_string();
        #[cfg(not(target_os = "android"))]
        return "".to_string();
    }
}

// 全局配置辅助函数
fn get_id() -> String {
    hbb_common::config::Config::get_id()
}

fn change_id(id: String) {
    ui_interface::change_id(id);
}

fn get_options() -> String {
    ui_interface::get_options()
}

fn set_options(options: HashMap<String, String>) {
    ui_interface::set_options(options);
}

fn get_local_option(key: String) -> String {
    ui_interface::get_local_option(key)
}

fn set_local_option(key: String, value: String) {
    ui_interface::set_local_option(key, value);
}

// 系统信息辅助函数
fn get_version() -> String {
    crate::get_version()
}

fn get_app_name() -> String {
    crate::get_app_name()
}

fn get_license() -> String {
    crate::get_license()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_isServiceClipboardEnabled(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if crate::clipboard_service::is_enabled() {
        1
    } else {
        0
    }
}

// 会话输入相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionInputKey(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    name: JString,
    down: jboolean,
    press: jboolean,
    alt: jboolean,
    ctrl: jboolean,
    shift: jboolean,
    command: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let name: String = match env.get_string(name) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get name string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 获取会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.input_key(
            &name,
            down != 0,
            press != 0,
            alt != 0,
            ctrl != 0,
            shift != 0,
            command != 0,
        );
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionInputString(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    value: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let value: String = match env.get_string(value) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get value string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 获取会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.input_string(&value);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionLockScreen(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 获取会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.lock_screen();
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionCtrlAltDel(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 获取会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.ctrl_alt_del();
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionReconnect(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    force_relay: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 获取会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.reconnect(force_relay != 0);
    }
}

// 会话管理相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionAddSync(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    id: JString,
    is_file_transfer: jboolean,
    is_port_forward: jboolean,
    is_rdp: jboolean,
    switch_uuid: JString,
    force_relay: jboolean,
    password: JString,
    is_shared_password: jboolean,
) -> jstring {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return env.new_string("Failed to get session_id").unwrap().into_raw();
        }
    };
    
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return env.new_string("Failed to get id").unwrap().into_raw();
        }
    };
    
    let switch_uuid: String = match env.get_string(switch_uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get switch_uuid string: {:?}", e);
            return env.new_string("Failed to get switch_uuid").unwrap().into_raw();
        }
    };
    
    let password: String = match env.get_string(password) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get password string: {:?}", e);
            return env.new_string("Failed to get password").unwrap().into_raw();
        }
    };
    
    // 添加会话
    match session_add(
        session_id,
        id,
        is_file_transfer != 0,
        is_port_forward != 0,
        is_rdp != 0,
        switch_uuid,
        force_relay != 0,
        password,
        is_shared_password != 0,
    ) {
        Ok(_) => env.new_string("").unwrap().into_raw(),
        Err(e) => env.new_string(format!("Failed to add session: {}", e)).unwrap().into_raw(),
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionStart(
    env: JNIEnv,
    _class: JClass,
    callback: JObject,
    session_id: JString,
    id: JString,
) -> jboolean {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return 0;
        }
    };
    
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return 0;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return 0;
        }
    };
    
    let callback = env.new_global_ref(callback).unwrap();
    
    // 创建事件回调
    let event_callback = Box::new(move |event: EventToUI| {
        let env = match get_jvm().attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                log::error!("Failed to attach JVM thread: {:?}", e);
                return;
            }
        };
        
        match event {
            EventToUI::Event(event) => {
                let event_jstring = match env.new_string(event) {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("Failed to create Java string: {:?}", e);
                        return;
                    }
                };
                
                let callback_obj = callback.as_obj();
                let _ = env.call_method(
                    callback_obj,
                    "onEvent",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(event_jstring.into())],
                );
            }
            EventToUI::Rgba(display) => {
                let callback_obj = callback.as_obj();
                let _ = env.call_method(
                    callback_obj,
                    "onRgba",
                    "(I)V",
                    &[JValue::Int(display as i32)],
                );
            }
            EventToUI::Texture(display, gpu_texture) => {
                let callback_obj = callback.as_obj();
                let _ = env.call_method(
                    callback_obj,
                    "onTexture",
                    "(IZ)V",
                    &[JValue::Int(display as i32), JValue::Bool(gpu_texture as u8)],
                );
            }
        }
    });
    
    // 启动会话
    match session_start(session_id, id) {
        Ok(_) => 1,
        Err(e) => {
            log::error!("Failed to start session: {:?}", e);
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getUuid(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let uuid = get_uuid();
    env.new_string(uuid).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getVersion(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let version = get_version();
    env.new_string(version).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getOptions(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let options = get_options();
    env.new_string(options).unwrap().into_raw()
}

// 文件传输相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_readLocalDir(
    env: JNIEnv,
    _class: JClass,
    path: JString,
    show_hidden: jboolean,
) -> jstring {
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    if let Ok(fd) = fs::read_dir(&fs::get_path(&path), show_hidden != 0) {
        let json = make_fd_to_json(fd.id, path, &fd.entries);
        env.new_string(json).unwrap().into_raw()
    } else {
        env.new_string("").unwrap().into_raw()
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_readLocalEmptyDirsRecursive(
    env: JNIEnv,
    _class: JClass,
    path: JString,
    include_hidden: jboolean,
) -> jstring {
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    if let Ok(fds) = fs::get_empty_dirs_recursive(&path, include_hidden != 0) {
        let json = make_vec_fd_to_json(&fds);
        env.new_string(json).unwrap().into_raw()
    } else {
        env.new_string("").unwrap().into_raw()
    }
}

// 语言和主题相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_changeLanguage(
    env: JNIEnv,
    _class: JClass,
    lang: JString,
) {
    let lang: String = match env.get_string(lang) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get lang string: {:?}", e);
            return;
        }
    };
    
    change_language(lang);
}

// 测试服务器连接
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_testIfValidServer(
    env: JNIEnv,
    _class: JClass,
    server: JString,
    test_with_proxy: jboolean,
) -> jstring {
    let server: String = match env.get_string(server) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get server string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let result = test_if_valid_server(server, test_with_proxy != 0);
    env.new_string(result).unwrap().into_raw()
}

// 代理设置
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setSocks(
    env: JNIEnv,
    _class: JClass,
    proxy: JString,
    username: JString,
    password: JString,
) {
    let proxy: String = match env.get_string(proxy) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get proxy string: {:?}", e);
            return;
        }
    };
    
    let username: String = match env.get_string(username) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get username string: {:?}", e);
            return;
        }
    };
    
    let password: String = match env.get_string(password) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get password string: {:?}", e);
            return;
        }
    };
    
    set_socks(proxy, username, password);
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getProxyStatus(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if get_proxy_status() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getSocks(
    env: JNIEnv,
    _class: JClass,
) -> jobjectArray {
    let socks = get_socks();
    let size = socks.len() as i32;
    
    let string_class = env.find_class("java/lang/String").unwrap();
    let result = env.new_object_array(size, string_class, JObject::null()).unwrap();
    
    for (i, sock) in socks.iter().enumerate() {
        let j_sock = env.new_string(sock).unwrap();
        env.set_object_array_element(result, i as i32, j_sock).unwrap();
    }
    
    result
}

// 发现局域网设备
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_discover(
    _env: JNIEnv,
    _class: JClass,
) {
    discover();
}

// 处理中继ID
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_handleRelayId(
    env: JNIEnv,
    _class: JClass,
    id: JString,
) -> jstring {
    let id: String = match env.get_string(id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get id string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let result = handle_relay_id(&id).to_owned();
    env.new_string(result).unwrap().into_raw()
}

// HTTP请求
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_httpRequest(
    env: JNIEnv,
    _class: JClass,
    url: JString,
    method: JString,
    body: JString,
    header: JString,
) {
    let url: String = match env.get_string(url) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get url string: {:?}", e);
            return;
        }
    };
    
    let method: String = match env.get_string(method) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get method string: {:?}", e);
            return;
        }
    };
    
    let body: String = match env.get_string(body) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get body string: {:?}", e);
            return;
        }
    };
    
    let header: String = match env.get_string(header) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get header string: {:?}", e);
            return;
        }
    };
    
    let body_option = if body.is_empty() { None } else { Some(body) };
    http_request(url, method, body_option, header);
}

// 获取HTTP状态
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getHttpStatus(
    env: JNIEnv,
    _class: JClass,
    url: JString,
) -> jstring {
    let url: String = match env.get_string(url) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get url string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    if let Some(status) = get_async_http_status(url) {
        env.new_string(status).unwrap().into_raw()
    } else {
        env.new_string("").unwrap().into_raw()
    }
}

// 获取异步任务状态
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getAsyncStatus(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let status = get_async_job_status();
    env.new_string(status).unwrap().into_raw()
}

// 获取错误信息
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getError(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let error = get_error();
    env.new_string(error).unwrap().into_raw()
}

// 获取API服务器
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getApiServer(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let server = get_api_server();
    env.new_string(server).unwrap().into_raw()
}

// 检查是否使用公共服务器
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_isUsingPublicServer(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if crate::using_public_server() {
        1
    } else {
        0
    }
}

// 获取连接状态
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getConnectStatus(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let mut state = hbb_common::config::get_online_state();
    if state > 0 {
        state = 1;
    }
    let status = serde_json::json!({ "status_num": state }).to_string();
    env.new_string(status).unwrap().into_raw()
}

// 检查连接状态
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_checkConnectStatus(
    _env: JNIEnv,
    _class: JClass,
) {
    // 在Android上不需要实现
}

// 获取登录设备信息
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getLoginDeviceInfo(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let info = get_login_device_info_json();
    env.new_string(info).unwrap().into_raw()
}

// 获取URI前缀
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getUriPrefix(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let prefix = crate::get_uri_prefix();
    env.new_string(prefix).unwrap().into_raw()
}

// 获取用户默认选项
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_getUserDefaultOption(
    env: JNIEnv,
    _class: JClass,
    key: JString,
) -> jstring {
    let key: String = match env.get_string(key) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get key string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let value = get_user_default_option(key);
    env.new_string(value).unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setUserDefaultOption(
    env: JNIEnv,
    _class: JClass,
    key: JString,
    value: JString,
) {
    let key: String = match env.get_string(key) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get key string: {:?}", e);
            return;
        }
    };
    
    let value: String = match env.get_string(value) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get value string: {:?}", e);
            return;
        }
    };
    
    set_user_default_option(key, value);
}

// 文件管理相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionReadRemoteDir(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    path: JString,
    include_hidden: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 读取远程目录
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.read_remote_dir(path, include_hidden != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSendFiles(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    act_id: jint,
    path: JString,
    to: JString,
    file_num: jint,
    include_hidden: jboolean,
    is_remote: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return;
        }
    };
    
    let to: String = match env.get_string(to) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get to string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 发送文件
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.send_files(act_id as i32, path, to, file_num as i32, include_hidden != 0, is_remote != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionAddJob(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    act_id: jint,
    path: JString,
    to: JString,
    file_num: jint,
    include_hidden: jboolean,
    is_remote: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return;
        }
    };
    
    let to: String = match env.get_string(to) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get to string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 添加任务
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.add_job(act_id as i32, path, to, file_num as i32, include_hidden != 0, is_remote != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionCancelJob(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    act_id: jint,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 取消任务
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.cancel_job(act_id as i32);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionRemoveFile(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    act_id: jint,
    path: JString,
    file_num: jint,
    is_remote: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 删除文件
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.remove_file(act_id as i32, path, file_num as i32, is_remote != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionReadDirToRemoveRecursive(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    act_id: jint,
    path: JString,
    is_remote: jboolean,
    show_hidden: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 递归删除目录
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.remove_dir_all(act_id as i32, path, is_remote != 0, show_hidden != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionCreateDir(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    act_id: jint,
    path: JString,
    is_remote: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 创建目录
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.create_dir(act_id as i32, path, is_remote != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionReadLocalDirSync(
    env: JNIEnv,
    _class: JClass,
    path: JString,
    show_hidden: jboolean,
) -> jstring {
    let path: String = match env.get_string(path) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get path string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    // 读取本地目录
    if let Ok(fd) = fs::read_dir(&fs::get_path(&path), show_hidden != 0) {
        let json = make_fd_to_json(fd.id, path, &fd.entries);
        env.new_string(json).unwrap().into_raw()
    } else {
        env.new_string("").unwrap().into_raw()
    }
}

// 端口转发相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionAddPortForward(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    local_port: jint,
    remote_host: JString,
    remote_port: jint,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let remote_host: String = match env.get_string(remote_host) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get remote_host string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 添加端口转发
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.add_port_forward(local_port as i32, remote_host, remote_port as i32);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionRemovePortForward(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    local_port: jint,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 移除端口转发
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.remove_port_forward(local_port as i32);
    }
}

// 语音通话相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionRequestVoiceCall(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 请求语音通话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.request_voice_call();
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionCloseVoiceCall(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 关闭语音通话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.close_voice_call();
    }
}

// RDP相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionNewRdp(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 创建新的RDP会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.new_rdp();
    }
}

// 会话切换相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSwitchSides(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 切换会话两端
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.switch_sides();
    }
}

// 会话权限提升相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionElevateDirect(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 直接提升权限
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.elevate_direct();
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionElevateWithLogon(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    username: JString,
    password: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let username: String = match env.get_string(username) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get username string: {:?}", e);
            return;
        }
    };
    
    let password: String = match env.get_string(password) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get password string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 使用登录凭证提升权限
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.elevate_with_logon(username, password);
    }
}

// 会话分辨率相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionChangeResolution(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    display: jint,
    width: jint,
    height: jint,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 更改分辨率
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.change_resolution(display, width, height);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSetSize(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    display: jint,
    width: jint,
    height: jint,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 设置大小
    super::flutter::session_set_size(session_id, display as usize, width as usize, height as usize);
}

// 会话选择相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSendSelectedSessionId(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    selected_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let selected_id: String = match env.get_string(selected_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get selected_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 发送选择的会话ID
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.send_selected_session_id(selected_id);
    }
}

// 会话记录相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionRecordScreen(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    start: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 记录屏幕
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.record_screen(start != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionGetIsRecording(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) -> jboolean {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return 0;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return 0;
        }
    };
    
    // 获取是否正在记录
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        if session.is_recording() {
            1
        } else {
            0
        }
    } else {
        0
    }
}

// 会话刷新相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionRefresh(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    display: jint,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 刷新会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.refresh_video(display as _);
    }
}

// 会话关闭相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionClose(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 关闭会话
    if let Some(session) = sessions::remove_session_by_session_id(&session_id) {
        // 在移动平台上，我们仍然调用这个方法以确保代码的稳定性
        crate::keyboard::release_remote_keys("map");
        session.close_event_stream(session_id);
        session.close();
    }
}

// 全局事件流相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_startGlobalEventStream(
    env: JNIEnv,
    _class: JClass,
    callback: JObject,
    app_type: JString,
) -> jboolean {
    let app_type: String = match env.get_string(app_type) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_type string: {:?}", e);
            return 0;
        }
    };
    
    let callback = match env.new_global_ref(callback) {
        Ok(global_ref) => global_ref,
        Err(e) => {
            log::error!("Failed to create global reference: {:?}", e);
            return 0;
        }
    };
    
    // 创建一个自定义的 StreamSink 实现
    let sink = AndroidEventSink::new(callback);
    
    // 启动全局事件流
    match flutter::start_global_event_stream(Box::new(sink), app_type) {
        Ok(_) => 1,
        Err(e) => {
            log::error!("Failed to start global event stream: {:?}", e);
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_stopGlobalEventStream(
    env: JNIEnv,
    _class: JClass,
    app_type: JString,
) {
    let app_type: String = match env.get_string(app_type) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get app_type string: {:?}", e);
            return;
        }
    };
    
    // 停止全局事件流
    flutter::stop_global_event_stream(app_type);
}

// 实现一个自定义的StreamSink，将事件转发到Java回调
struct AndroidEventSink {
    callback: GlobalRef,
}

impl AndroidEventSink {
    fn new(callback: GlobalRef) -> Self {
        Self { callback }
    }
}

impl StreamSink<String> for AndroidEventSink {
    fn add(&self, event: String) {
        let env = match JNIEnv::attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                log::error!("Failed to attach JNI thread: {:?}", e);
                return;
            }
        };
        
        let j_event = match env.new_string(event) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to create Java string: {:?}", e);
                return;
            }
        };
        
        // 调用Java回调方法
        let _ = env.call_method(
            self.callback.as_obj(),
            "onEvent",
            "(Ljava/lang/String;)V",
            &[JValue::Object(j_event.into())],
        );
        
        if let Err(e) = env.exception_check() {
            log::error!("Exception occurred during callback: {:?}", e);
            let _ = env.exception_clear();
        }
    }
    
    fn add_sink(&self, _sink: Box<dyn StreamSink<String> + Send + 'static>) {
        // 在Android上不需要实现
    }
}

// 会话相关方法
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionGetToggleOption(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    arg: JString,
) -> jboolean {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return 0;
        }
    };
    
    let arg: String = match env.get_string(arg) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get arg string: {:?}", e);
            return 0;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return 0;
        }
    };
    
    // 获取会话选项
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        if session.get_toggle_option(arg) {
            1
        } else {
            0
        }
    } else {
        0
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionGetOption(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    arg: JString,
) -> jstring {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let arg: String = match env.get_string(arg) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get arg string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    // 获取会话选项
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        let option = session.get_option(arg);
        env.new_string(option).unwrap().into_raw()
    } else {
        env.new_string("").unwrap().into_raw()
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionLogin(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    os_username: JString,
    os_password: JString,
    password: JString,
    remember: jboolean,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let os_username: String = match env.get_string(os_username) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get os_username string: {:?}", e);
            return;
        }
    };
    
    let os_password: String = match env.get_string(os_password) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get os_password string: {:?}", e);
            return;
        }
    };
    
    let password: String = match env.get_string(password) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get password string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 登录会话
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.login(os_username, os_password, password, remember != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionToggleOption(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    value: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let value: String = match env.get_string(value) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get value string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 切换会话选项
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        log::warn!("toggle option {}", &value);
        session.toggle_option(value.clone());
        try_sync_peer_option(&session, &session_id, &value, None);
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionGetFlutterOption(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    k: JString,
) -> jstring {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let k: String = match env.get_string(k) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get k string: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return env.new_string("").unwrap().into_raw();
        }
    };
    
    // 获取Flutter选项
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        let option = session.get_flutter_option(k);
        env.new_string(option).unwrap().into_raw()
    } else {
        env.new_string("").unwrap().into_raw()
    }
}

#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sessionSetFlutterOption(
    env: JNIEnv,
    _class: JClass,
    session_id: JString,
    k: JString,
    v: JString,
) {
    let session_id: String = match env.get_string(session_id) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get session_id string: {:?}", e);
            return;
        }
    };
    
    let k: String = match env.get_string(k) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get k string: {:?}", e);
            return;
        }
    };
    
    let v: String = match env.get_string(v) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("Failed to get v string: {:?}", e);
            return;
        }
    };
    
    let session_id = match uuid::Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            log::error!("Failed to parse session_id as UUID: {:?}", e);
            return;
        }
    };
    
    // 设置Flutter选项
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        session.save_flutter_option(k, v);
    }
}

// 添加更多的JNI方法实现...