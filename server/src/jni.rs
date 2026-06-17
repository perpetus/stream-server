use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use once_cell::sync::Lazy;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use crate::{ServerConfig, ServerHandle};

static SERVER_HANDLE: Lazy<Mutex<Option<ServerHandle>>> = Lazy::new(|| Mutex::new(None));

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_com_stremio_mobile_server_JniStreamingServerController_startServerNative(
    mut env: JNIEnv,
    _class: JClass,
    config_dir: JString,
    cache_dir: JString,
    port: jni::sys::jint,
) -> jstring {
    let config_dir_str: String = match env.get_string(&config_dir) {
        Ok(s) => s.into(),
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", "Invalid configDir string");
            return std::ptr::null_mut();
        }
    };

    let cache_dir_str: String = match env.get_string(&cache_dir) {
        Ok(s) => s.into(),
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", "Invalid cacheDir string");
            return std::ptr::null_mut();
        }
    };

    let mut handle_lock = SERVER_HANDLE.lock().unwrap();
    if handle_lock.is_some() {
        let bound_addr = handle_lock.as_ref().unwrap().bound_http_addr();
        let url = format!("http://{}", bound_addr);
        return env.new_string(url).unwrap().into_raw();
    }

    let mut cfg = ServerConfig::embedded();
    cfg.config_dir = Some(PathBuf::from(config_dir_str));
    cfg.cache_dir = Some(PathBuf::from(cache_dir_str));
    cfg.http_addr = SocketAddr::from(([127, 0, 0, 1], port as u16));
    cfg.init_logging = true;

    match crate::start(cfg) {
        Ok(handle) => {
            let bound_addr = handle.bound_http_addr();
            let url = format!("http://{}", bound_addr);
            *handle_lock = Some(handle);
            env.new_string(url).unwrap().into_raw()
        }
        Err(err) => {
            let err_msg = format!("Failed to start server: {}", err);
            let _ = env.throw_new("java/lang/RuntimeException", err_msg);
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_com_stremio_mobile_server_JniStreamingServerController_stopServerNative(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut handle_lock = SERVER_HANDLE.lock().unwrap();
    if let Some(handle) = handle_lock.take() {
        let _ = handle.shutdown();
        let _ = handle.join();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_com_stremio_mobile_server_JniStreamingServerController_getServerUrlNative(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let handle_lock = SERVER_HANDLE.lock().unwrap();
    if let Some(handle) = handle_lock.as_ref() {
        let bound_addr = handle.bound_http_addr();
        let url = format!("http://{}", bound_addr);
        env.new_string(url).unwrap().into_raw()
    } else {
        std::ptr::null_mut()
    }
}
