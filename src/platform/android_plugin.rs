use manganis::{
    android::with_activity,
    jni::objects::{GlobalRef, JClass, JValue},
};

// The Kotlin package may differ from Gradle's namespace, so resolve via Activity's loader.
pub fn load_activity_plugin(class_name: &str, plugin_label: &str) -> Result<GlobalRef, String> {
    let inner_result = with_activity(|env, activity| {
        Some(try {
            let class_loader = env
                .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
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
            let plugin_class = JClass::from(
                env.call_method(
                    &class_loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&class_name_jstring)],
                )
                .map_err(|error| {
                    format!("Failed to load {plugin_label} class {class_name:?}: {error:?}")
                })?
                .l()
                .map_err(|error| {
                    format!("Failed to read class object for {plugin_label}: {error:?}")
                })?,
            );
            let instance = env
                .new_object(
                    &plugin_class,
                    "(Landroid/app/Activity;)V",
                    &[JValue::Object(activity)],
                )
                .map_err(|error| format!("Failed to construct {plugin_label}: {error:?}"))?;
            env.new_global_ref(&instance)
                .map_err(|error| format!("Failed to create {plugin_label} global ref: {error:?}"))?
        })
    });

    inner_result.unwrap_or_else(|| Err("Failed to get JNI environment".to_string()))
}
