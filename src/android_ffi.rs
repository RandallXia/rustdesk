use crate::{flutter_ffi::EventToUI, ui_interface::*};
use hbb_common::{bail, config::LocalConfig, log, ResultType};
use jni::{objects::JObject, JNIEnv};
use lazy_static::lazy_static;
use serde_json::json;
use std::{collections::HashMap, sync::RwLock};

// 应用类型常量
pub(crate) const APP_TYPE_MAIN: &str = "main";
pub(crate) const APP_TYPE_CM: &str = "main"; // 在Android上，CM使用与main相同的通道

// 全局事件回调注册表
lazy_static! {
    static ref GLOBAL_EVENT_CALLBACKS: RwLock<HashMap<String, AndroidEventCallback>> =
        Default::default();
}

// 用于保存Android JNI回调信息的结构
pub struct AndroidEventCallback {
    callback_obj: jni::objects::GlobalRef,
}

// AndroidEventCallback的实现，用于JNI事件处理
impl AndroidEventCallback {
    pub fn new(env: &mut JNIEnv, callback_obj: JObject) -> ResultType<Self> {
        let callback_obj = env.new_global_ref(callback_obj)?;
        Ok(Self { callback_obj })
    }

    pub fn send_event(&self, event: String) -> bool {
        let res = if let Some(jvm) = scrap::android::ffi::JVM.read().unwrap().as_ref() {
            jvm.attach_current_thread()
                .and_then(|mut env| {
                    env.call_method(
                        &self.callback_obj,
                        "onEvent",
                        "(Ljava/lang/String;)V",
                        &[env.new_string(event)?.into()],
                    )
                    .map(|_| true)
                })
                .unwrap_or_else(|e| {
                    log::error!("通过JNI发送事件失败: {:?}", e);
                    false
                })
        } else {
            log::error!("无法获取JavaVM实例");
            false
        };
        res
    }
}

// 为特定应用类型注册全局事件回调
#[no_mangle]
pub extern "C" fn register_global_event_callback(
    env: JNIEnv,
    _: JObject,
    app_type: jni::objects::JString,
    callback: JObject,
) -> jni::sys::jboolean {
    let mut env = env;
    let result = (|| -> ResultType<bool> {
        let app_type: String = env.get_string(&app_type)?.into();
        let app_type_values: Vec<&str> = app_type.split(',').collect();
        
        let callback = AndroidEventCallback::new(&mut env, callback)?;
        
        let mut lock = GLOBAL_EVENT_CALLBACKS.write().unwrap();
        if !lock.contains_key(app_type_values[0]) {
            lock.insert(app_type_values[0].to_string(), callback);
        } else {
            lock.insert(app_type.clone(), callback);
            log::warn!(
                "Global event callback of type {} is registered before, but now replaced",
                app_type
            );
        }
        Ok(true)
    })();
    
    match result {
        Ok(true) => 1 as jni::sys::jboolean,
        _ => 0 as jni::sys::jboolean,
    }
}

// 注销特定应用类型的全局事件回调
#[no_mangle]
pub extern "C" fn unregister_global_event_callback(
    mut env: JNIEnv,
    _: JObject,
    app_type: jni::objects::JString,
) -> jni::sys::jboolean {
    let result = (|| -> ResultType<bool> {
        let app_type: String = env.get_string(&app_type)?.into();
        let _ = GLOBAL_EVENT_CALLBACKS.write().unwrap().remove(&app_type);
        Ok(true)
    })();
    
    match result {
        Ok(true) => 1 as jni::sys::jboolean,
        _ => 0 as jni::sys::jboolean,
    }
}

// 向特定通道推送全局事件
#[inline]
pub fn push_global_event(channel: &str, event: String) -> Option<bool> {
    GLOBAL_EVENT_CALLBACKS
        .read()
        .unwrap()
        .get(channel)
        .map(|callback| callback.send_event(event))
}

// 获取所有已注册的全局事件通道
#[inline]
pub fn get_global_event_channels() -> Vec<String> {
    GLOBAL_EVENT_CALLBACKS
        .read()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

// Android的服务器端连接管理器
pub mod connection_manager {
    use std::collections::HashMap;

    use hbb_common::log;
    use scrap::android::call_main_service_set_by_name;
    use serde_json::json;

    use crate::ui_cm_interface::InvokeUiCM;

    use super::{push_global_event, APP_TYPE_CM};

    #[derive(Clone)]
    struct AndroidHandler {}

    impl InvokeUiCM for AndroidHandler {
        fn add_connection(&self, client: &crate::ui_cm_interface::Client) {
            let client_json = serde_json::to_string(&client).unwrap_or("".into());
            // 发送到Android服务，无论UI是否显示都激活通知。
            if let Err(e) =
                call_main_service_set_by_name("add_connection", Some(&client_json), None)
            {
                log::debug!("call_main_service_set_by_name fail,{}", e);
            }
            // 发送到UI，刷新小部件
            self.push_event("add_connection", &[("client", &client_json)]);
        }

        fn remove_connection(&self, id: i32, close: bool) {
            self.push_event(
                "on_client_remove",
                &[("id", &id.to_string()), ("close", &close.to_string())],
            );
        }

        fn new_message(&self, id: i32, text: String) {
            self.push_event(
                "chat_server_mode",
                &[("id", &id.to_string()), ("text", &text)],
            );
        }

        fn change_theme(&self, dark: String) {
            self.push_event("theme", &[("dark", &dark)]);
        }

        fn change_language(&self) {
            self.push_event::<&str>("language", &[]);
        }

        fn show_elevation(&self, show: bool) {
            self.push_event("show_elevation", &[("show", &show.to_string())]);
        }

        fn update_voice_call_state(&self, client: &crate::ui_cm_interface::Client) {
            let client_json = serde_json::to_string(&client).unwrap_or("".into());
            // 发送到Android服务，无论UI是否显示都激活通知。
            if let Err(e) =
                call_main_service_set_by_name("update_voice_call_state", Some(&client_json), None)
            {
                log::debug!("call_main_service_set_by_name fail,{}", e);
            }
            self.push_event("update_voice_call_state", &[("client", &client_json)]);
        }

        fn file_transfer_log(&self, action: &str, log: &str) {
            self.push_event("cm_file_transfer_log", &[(action, log)]);
        }
    }

    impl AndroidHandler {
        fn push_event<V>(&self, name: &str, event: &[(&str, V)])
        where
            V: Sized + serde::Serialize + Clone,
        {
            let mut h: HashMap<&str, serde_json::Value> =
                event.iter().map(|(k, v)| (*k, json!(*v))).collect();
            debug_assert!(h.get("name").is_none());
            h.insert("name", json!(name));

            let event_json = serde_json::ser::to_string(&h).unwrap_or("".to_owned());
            let _ = push_global_event(APP_TYPE_CM, event_json);
        }
    }

    pub fn cm_init() {
        // Android CM初始化由Android服务处理
    }

    pub fn start_channel(
        rx: hbb_common::tokio::sync::mpsc::UnboundedReceiver<crate::ipc::Data>,
        tx: hbb_common::tokio::sync::mpsc::UnboundedSender<crate::ipc::Data>,
    ) {
        use crate::ui_cm_interface::start_listen;
        let cm = crate::ui_cm_interface::ConnectionManager {
            ui_handler: AndroidHandler {},
        };
        std::thread::spawn(move || start_listen(cm, rx, tx));
    }
}

// Android的会话处理
pub struct AndroidSessionHandler {
    event_callback: Option<AndroidEventCallback>,
    displays: Vec<usize>,
}

impl Default for AndroidSessionHandler {
    fn default() -> Self {
        Self {
            event_callback: None,
            displays: Vec::new(),
        }
    }
}

// 注册会话事件回调
#[no_mangle]
pub extern "C" fn register_session_event_callback(
    mut env: JNIEnv,
    _: JObject,
    session_id: jni::objects::JString,
    callback: JObject,
) -> jni::sys::jboolean {
    let mut env = env;
    let result = (|| -> ResultType<bool> {
        let session_id_str: String = env.get_string(&session_id)?.into();
        let session_id = uuid::Uuid::parse_str(&session_id_str)?;
        
        let callback = AndroidEventCallback::new(&mut env, callback)?;
        
        if let Some(session) = crate::flutter::sessions::get_session_by_session_id(&session_id) {
            let mut handlers = session.session_handlers.write().unwrap();
            if let Some(handler) = handlers.get_mut(&session_id) {
                if let Some(android_handler) = handler.downcast_mut::<AndroidSessionHandler>() {
                    android_handler.event_callback = Some(callback);
                    return Ok(true);
                }
            }
        }
        bail!("Session not found or handler not compatible")
    })();
    
    match result {
        Ok(true) => 1 as jni::sys::jboolean,
        _ => 0 as jni::sys::jboolean,
    }
}

// 向会话发送事件
pub fn send_event_to_ui(session_id: &uuid::Uuid, event: EventToUI) -> bool {
    if let Some(session) = crate::flutter::sessions::get_session_by_session_id(session_id) {
        let handlers = session.session_handlers.read().unwrap();
        if let Some(handler) = handlers.get(session_id) {
            if let Some(android_handler) = handler.downcast_ref::<AndroidSessionHandler>() {
                if let Some(callback) = &android_handler.event_callback {
                    let event_str = match event {
                        EventToUI::Event(s) => s,
                        EventToUI::Rgba(display) => format!("{{\"type\":\"rgba\",\"display\":{}}}", display),
                        EventToUI::Texture(display, gpu_texture) => {
                            format!("{{\"type\":\"texture\",\"display\":{},\"gpu_texture\":{}}}", display, gpu_texture)
                        }
                    };
                    return callback.send_event(event_str);
                }
            }
        }
    }
    false
}

// 使用Android回调启动会话
pub fn session_start_with_android_callback(
    session_id: &uuid::Uuid,
    id: &str,
    callback: AndroidEventCallback,
) -> ResultType<()> {
    let mut is_connected = false;
    let mut is_found = false;
    
    for s in crate::flutter::sessions::get_sessions() {
        let mut handlers = s.session_handlers.write().unwrap();
        if let Some(handler) = handlers.get_mut(session_id) {
            if let Some(android_handler) = handler.downcast_mut::<AndroidSessionHandler>() {
                is_connected = android_handler.event_callback.is_some();
                // 如果存在现有回调，则发送关闭事件
                if let Some(existing_callback) = &android_handler.event_callback {
                    let _ = existing_callback.send_event("close".to_owned());
                }
                android_handler.event_callback = Some(callback);
                is_found = true;
                break;
            }
        }
    }
    
    if !is_found {
        bail!(
            "No session with peer id {}, session id: {}",
            id,
            session_id.to_string()
        );
    }

    if let Some(session) = crate::flutter::sessions::get_session_by_session_id(session_id) {
        let is_first_ui_session = session.session_handlers.read().unwrap().len() == 1;
        if !is_connected && is_first_ui_session {
            log::info!("Session {} start", id);
            let session = (*session).clone();
            std::thread::spawn(move || {
                let round = session.connection_round_state.lock().unwrap().new_round();
                crate::flutter::io_loop(session, round);
            });
        }
        Ok(())
    } else {
        bail!("No session with peer id {}", id)
    }
}

// 启动会话的JNI入口点
#[no_mangle]
pub extern "C" fn start_session(
    mut env: JNIEnv,
    _: JObject,
    session_id: jni::objects::JString,
    peer_id: jni::objects::JString,
    callback: JObject,
) -> jni::sys::jboolean {
    let mut env = env;
    let result = (|| -> ResultType<bool> {
        let session_id_str: String = env.get_string(&session_id)?.into();
        let session_id = uuid::Uuid::parse_str(&session_id_str)?;
        let peer_id: String = env.get_string(&peer_id)?.into();
        
        let callback = AndroidEventCallback::new(&mut env, callback)?;
        session_start_with_android_callback(&session_id, &peer_id, callback)?;
        Ok(true)
    })();
    
    match result {
        Ok(true) => 1 as jni::sys::jboolean,
        _ => 0 as jni::sys::jboolean,
    }
}