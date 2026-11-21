//! JNI activity access for the Android plugin layer.
//!
//! `ndk-context` carries the JavaVM + the NativeActivity's global reference
//! (installed by the NativeActivity glue); this module turns them into a
//! scoped `Env` + `Activity` pair for the plugin loaders.
//!
//! Isolated `unsafe` (raw `ndk-context` pointers) in a module-level
//! `#![allow(unsafe_code)]`, same pattern as `platform::native_bridge`: the
//! crate denies `unsafe_code`, and this is the only place raw JNI context
//! pointers are unwrapped.
#![allow(unsafe_code)]

use jni::jni_sig;
use jni::jni_str;
use jni::objects::{Global, JObject, JValue};
use jni::{Env, JavaVM};

/// Runs `f` with the activity's env + activity reference; `None` when the
/// JNI context is not installed yet (non-activity process or early boot).
pub fn with_activity<R>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> Option<R>) -> Option<R> {
    let context = ndk_context::android_context();
    let vm_raw = context.vm().cast::<jni::sys::JavaVM>();
    if vm_raw.is_null() {
        return None;
    }
    let activity_raw: jni::sys::jobject = context.context().cast::<std::ffi::c_void>() as _;
    if activity_raw.is_null() {
        return None;
    }
    // SAFETY: `ndk-context` guarantees both pointers outlive the call and the
    // VM is the process's only JavaVM.
    let vm = unsafe { JavaVM::from_raw(vm_raw) };
    vm.attach_current_thread(|env| {
        // SAFETY: the raw pointer is the NativeActivity glue's long-lived
        // global reference; the `JObject` wrapper is non-owning and never
        // deletes it, and it stays valid for the whole attachment.
        let activity = unsafe { JObject::from_raw(env, activity_raw) };
        Ok::<_, jni::errors::Error>(f(env, &activity))
    })
    .ok()
    .flatten()
}

/// Loads a Kotlin plugin class through the Activity's class loader and
/// constructs it with `(Activity)`; the Kotlin package may differ from
/// Gradle's namespace, so the loader is resolved via the Activity.
pub fn load_activity_plugin(
    class_name: &str,
    plugin_label: &str,
) -> Result<Global<JObject<'static>>, String> {
    with_activity(|env, activity| {
        Some(try {
            let class_loader = env
                .call_method(
                    activity,
                    jni_str!("getClassLoader"),
                    jni_sig!(() -> java.lang.ClassLoader),
                    &[],
                )
                .map_err(|error| {
                    format!(
                        "Failed to call Activity.getClassLoader() for {plugin_label}: {error:?}"
                    )
                })?
                .l()
                .map_err(|error| {
                    format!("Failed to read Activity class loader for {plugin_label}: {error:?}")
                })?;
            let class_name_jstring = env.new_string(class_name).map_err(|error| {
                format!("Failed to create class name string for {plugin_label}: {error:?}")
            })?;
            let class = env
                .call_method(
                    &class_loader,
                    jni_str!("loadClass"),
                    jni_sig!((name: java.lang.String) -> java.lang.Class),
                    &[JValue::Object(&class_name_jstring)],
                )
                .map_err(|error| {
                    format!("Failed to load {plugin_label} class {class_name:?}: {error:?}")
                })?
                .l()
                .map_err(|error| {
                    format!("Failed to read class object for {plugin_label}: {error:?}")
                })?;
            let class = env
                .as_cast::<jni::objects::JClass>(&class)
                .map_err(|error| {
                    format!("Failed to read class object for {plugin_label}: {error:?}")
                })?;
            let instance = env
                .new_object(
                    &*class,
                    jni_sig!((activity: android.app.Activity) -> void),
                    &[JValue::Object(activity)],
                )
                .map_err(|error| format!("Failed to construct {plugin_label}: {error:?}"))?;
            env.new_global_ref(&instance)
                .map_err(|error| format!("Failed to create {plugin_label} global ref: {error:?}"))?
        })
    })
    .unwrap_or_else(|| Err("Failed to get JNI environment".to_string()))
}
