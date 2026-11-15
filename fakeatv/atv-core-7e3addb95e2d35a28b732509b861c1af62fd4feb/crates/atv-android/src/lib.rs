//! JNI bindings for Android TV: runs AtvServer and dispatches callbacks into Kotlin/Java.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use once_cell::sync::Lazy;
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

use atv_core::{AtvConfig, AtvDelegate, AtvServer, EventKind, TouchPhase};

struct JniDelegate {
    jvm: JavaVM,
    callback: GlobalRef,
}

impl JniDelegate {
    fn new(jvm: JavaVM, callback: GlobalRef) -> Self {
        Self { jvm, callback }
    }

    fn check_and_clear_exception(env: &mut JNIEnv) {
        if let Ok(has_exception) = env.exception_check() {
            if has_exception {
                let _ = env.exception_clear();
            }
        }
    }

    fn call_void_string(&self, method: &str, arg: &str) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let jstr = match env.new_string(arg) {
                Ok(s) => s,
                Err(e) => {
                    warn!("failed to create JString: {e}");
                    return;
                }
            };
            let obj = self.callback.as_obj();
            let _ = env.call_method(
                obj,
                method,
                "(Ljava/lang/String;)V",
                &[JValue::Object(&jstr)],
            );
            Self::check_and_clear_exception(&mut env);
        }
    }
}

impl AtvDelegate for JniDelegate {
    fn on_button(&self, name: &str) {
        info!("[Android JNI] on_button: {name}");
        self.call_void_string("onButton", name);
    }

    fn on_touch(&self, dx: f64, dy: f64, phase: TouchPhase) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let phase_int = match phase {
                TouchPhase::Began => 1,
                TouchPhase::Moved => 2,
                TouchPhase::Ended => 4,
            };
            let obj = self.callback.as_obj();
            let _ = env.call_method(
                obj,
                "onTouch",
                "(DDI)V",
                &[
                    JValue::Double(dx),
                    JValue::Double(dy),
                    JValue::Int(phase_int),
                ],
            );
            Self::check_and_clear_exception(&mut env);
        }
    }

    fn on_audio(&self, volume: f64, muted: bool) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let obj = self.callback.as_obj();
            let _ = env.call_method(
                obj,
                "onAudio",
                "(DZ)V",
                &[JValue::Double(volume), JValue::Bool(muted as u8)],
            );
            Self::check_and_clear_exception(&mut env);
        }
    }

    fn on_event(&self, kind: EventKind, detail: &str) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let kind_str = match env.new_string(format!("{kind:?}")) {
                Ok(s) => s,
                Err(_) => return,
            };
            let detail_str = match env.new_string(detail) {
                Ok(s) => s,
                Err(_) => return,
            };
            let obj = self.callback.as_obj();
            let _ = env.call_method(
                obj,
                "onEvent",
                "(Ljava/lang/String;Ljava/lang/String;)V",
                &[JValue::Object(&kind_str), JValue::Object(&detail_str)],
            );
            Self::check_and_clear_exception(&mut env);
        }
    }

    fn on_text_input(&self, text: &str) {
        self.call_void_string("onTextInput", text);
    }

    fn on_media_command(&self, command: &str, param: i64) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let cmd_str = match env.new_string(command) {
                Ok(s) => s,
                Err(_) => return,
            };
            let obj = self.callback.as_obj();
            let _ = env.call_method(
                obj,
                "onMediaCommand",
                "(Ljava/lang/String;J)V",
                &[JValue::Object(&cmd_str), JValue::Long(param)],
            );
            Self::check_and_clear_exception(&mut env);
        }
    }
}

struct RunningServer {
    runtime: Runtime,
    server: Option<AtvServer>,
}

static SERVER_STATE: Lazy<Mutex<Option<RunningServer>>> = Lazy::new(|| Mutex::new(None));
static IS_RUNNING: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeStartServer(
    mut env: JNIEnv,
    _class: JClass,
    name: JString,
    pin: jint,
    device_id: JString,
    server_id: JString,
    local_ip: JString,
    callback: JObject,
) -> jboolean {
    let mut state_guard = match SERVER_STATE.lock() {
        Ok(guard) => guard,
        Err(_) => return JNI_FALSE,
    };

    if state_guard.is_some() {
        warn!("server is already running");
        return JNI_TRUE;
    }

    let device_name: String = match env.get_string(&name) {
        Ok(s) => s.into(),
        Err(_) => "Android TV".to_string(),
    };

    let device_id_str: Option<String> = env
        .get_string(&device_id)
        .ok()
        .map(|s| s.into())
        .filter(|s: &String| !s.is_empty());

    let server_id_str: Option<String> = env
        .get_string(&server_id)
        .ok()
        .map(|s| s.into())
        .filter(|s: &String| !s.is_empty());

    let local_ip_str: Option<String> = env
        .get_string(&local_ip)
        .ok()
        .map(|s| s.into())
        .filter(|s: &String| !s.is_empty());

    let jvm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            error!("failed to get JavaVM: {e}");
            return JNI_FALSE;
        }
    };

    let global_callback = match env.new_global_ref(callback) {
        Ok(g) => g,
        Err(e) => {
            error!("failed to create GlobalRef for callback: {e}");
            return JNI_FALSE;
        }
    };

    let runtime = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            error!("failed to create Tokio runtime: {e}");
            return JNI_FALSE;
        }
    };

    let delegate = Arc::new(JniDelegate::new(jvm, global_callback));
    let mut config = AtvConfig::default();
    config.name = device_name;
    config.pin = pin as u32;
    config.device_id = device_id_str;
    config.server_identifier = server_id_str;
    if let Some(ref s) = local_ip_str {
        if let Ok(ip) = s.parse::<Ipv4Addr>() {
            info!("using explicit local IP from Android: {ip}");
            config.ip = Some(ip);
        }
    }

    let server_res = runtime.block_on(async {
        AtvServer::start(config, delegate).await
    });

    match server_res {
        Ok(server) => {
            *state_guard = Some(RunningServer {
                runtime,
                server: Some(server),
            });
            IS_RUNNING.store(true, Ordering::SeqCst);
            info!("AtvServer successfully started on Android TV");
            JNI_TRUE
        }
        Err(e) => {
            error!("failed to start AtvServer: {e}");
            JNI_FALSE
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeStopServer(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut state_guard = match SERVER_STATE.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };

    if let Some(mut running) = state_guard.take() {
        if let Some(server) = running.server.take() {
            running.runtime.block_on(async {
                server.stop().await;
            });
        }
        IS_RUNNING.store(false, Ordering::SeqCst);
        info!("AtvServer stopped on Android TV");
    }
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeIsRunning(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if IS_RUNNING.load(Ordering::SeqCst) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeUpdateMediaInfo(
    mut env: JNIEnv,
    _class: JClass,
    title: JString,
    artist: JString,
    album: JString,
    app_name: JString,
    app_bundle_id: JString,
    duration_sec: jni::sys::jdouble,
    elapsed_sec: jni::sys::jdouble,
    playback_rate: jni::sys::jfloat,
    is_playing: jni::sys::jboolean,
    artwork_bytes: jni::objects::JByteArray,
) {
    let title_str = if !title.is_null() {
        env.get_string(&title).map(|s| s.into()).unwrap_or_default()
    } else {
        String::new()
    };
    let artist_str = if !artist.is_null() {
        env.get_string(&artist).map(|s| s.into()).unwrap_or_default()
    } else {
        String::new()
    };
    let album_str = if !album.is_null() {
        env.get_string(&album).map(|s| s.into()).unwrap_or_default()
    } else {
        String::new()
    };
    let app_name_str = if !app_name.is_null() {
        env.get_string(&app_name).map(|s| s.into()).unwrap_or_default()
    } else {
        String::new()
    };
    let app_bundle_id_str = if !app_bundle_id.is_null() {
        env.get_string(&app_bundle_id).map(|s| s.into()).unwrap_or_default()
    } else {
        String::new()
    };
    let is_playing_bool = is_playing != JNI_FALSE;

    let artwork = if !artwork_bytes.is_null() {
        match env.convert_byte_array(&artwork_bytes) {
            Ok(b) if !b.is_empty() => Some(b),
            _ => None,
        }
    } else {
        None
    };

    let media_info = atv_core::MediaInfo {
        title: title_str,
        artist: artist_str,
        album: album_str,
        app_name: app_name_str,
        app_bundle_id: app_bundle_id_str,
        duration: duration_sec,
        elapsed: elapsed_sec,
        rate: playback_rate,
        is_playing: is_playing_bool,
        artwork,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
    };

    if let Ok(guard) = SERVER_STATE.lock() {
        if let Some(ref running) = *guard {
            if let Some(ref server) = running.server {
                let hub = server.media_hub();
                running.runtime.spawn(async move {
                    hub.update(media_info).await;
                });
            }
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeGetAirplayPort(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    7000
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeGetCompanionPort(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    49153
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeReannounceMdns(
    _env: JNIEnv,
    _class: JClass,
) {
}

#[no_mangle]
pub extern "system" fn Java_com_corvofeng_fakeatv_AtvNative_nativeSetKeyboardFocus(
    _env: JNIEnv,
    _class: JClass,
    _focused: jboolean,
    _text: JString,
    _title: JString,
) {
}
