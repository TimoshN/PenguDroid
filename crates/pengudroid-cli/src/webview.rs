//! Load provider metadata with Android's own APK parser and signature verifier.
use super::*;

fn publish_file(path: &std::path::Path, bytes: &[u8]) -> Option<()> {
    // A different hosted app process may still have the previous ELF mapped.
    // Replace the directory entry instead of truncating its live mapping.
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes).ok()?;
    std::fs::rename(temp, path).ok()?;
    Some(())
}

static PAGE_PROBE: std::sync::Mutex<Option<(usize, std::time::Instant)>> =
    std::sync::Mutex::new(None);

pub(super) fn software_views(env: &JniEnv, activity: JObject) -> Option<()> {
    if std::env::var_os("PENGUDROID_WEBVIEW_SOFTWARE").is_none() {
        return Some(());
    }
    let activity_class = env.find_class("android/app/Activity")?;
    let window = env.method_id(activity_class, "getWindow", "()Landroid/view/Window;")?;
    let window = env.call_object(activity, window, &[])?;
    let window_class = env.find_class("android/view/Window")?;
    let decor = env.method_id(window_class, "getDecorView", "()Landroid/view/View;")?;
    let root = env.call_object(window, decor, &[])?;
    fn visit(env: &JniEnv, view: JObject, depth: usize) -> Option<()> {
        if view.is_null() || depth > 64 {
            return Some(());
        }
        let _frame = env.local_frame(16)?;
        let class = env.find_class("java/lang/Class")?;
        let instance = env.method_id(class, "isInstance", "(Ljava/lang/Object;)Z")?;
        let web = env.find_class("android/webkit/WebView")?;
        if env.call_boolean(web, instance, &[JValue { l: view }])? {
            let set = env.method_id(web, "setLayerType", "(ILandroid/graphics/Paint;)V")?;
            if !env.call_void(
                view,
                set,
                &[
                    JValue { i: 1 },
                    JValue {
                        l: std::ptr::null_mut(),
                    },
                ],
            ) {
                return None;
            }
            println!("webview: software Canvas layer selected");
        }
        let group = env.find_class("android/view/ViewGroup")?;
        if env.call_boolean(group, instance, &[JValue { l: view }])? {
            let count = env.method_id(group, "getChildCount", "()I")?;
            let get = env.method_id(group, "getChildAt", "(I)Landroid/view/View;")?;
            for i in 0..env.call_int(view, count, &[])? {
                visit(env, env.call_object(view, get, &[JValue { i }])?, depth + 1)?;
            }
        }
        Some(())
    }
    visit(env, root, 0)
}

pub(super) fn attach_probe(env: &JniEnv, thread: JObject) -> Option<()> {
    let probe = PAGE_PROBE.lock().unwrap_or_else(|e| e.into_inner());
    let (view, _) = (*probe)?;
    let class = env.find_class("android/app/ActivityThread")?;
    let activities = env.object_field(
        thread,
        env.field_id(class, "mActivities", "Landroid/util/ArrayMap;")?,
    );
    let map = env.find_class("android/util/ArrayMap")?;
    let value = env.method_id(map, "valueAt", "(I)Ljava/lang/Object;")?;
    let record = env.call_object(activities, value, &[JValue { i: 0 }])?;
    let rc = env.find_class("android/app/ActivityThread$ActivityClientRecord")?;
    let activity = env.object_field(
        record,
        env.field_id(rc, "activity", "Landroid/app/Activity;")?,
    );
    let ac = env.find_class("android/app/Activity")?;
    let content = env.method_id(ac, "setContentView", "(Landroid/view/View;)V")?;
    if !env.call_void(activity, content, &[JValue { l: view as JObject }]) {
        return None;
    }
    println!("webview: page probe attached to the activity window");
    Some(())
}

pub(super) fn probe_tick(env: &JniEnv) {
    let mut probe = PAGE_PROBE.lock().unwrap_or_else(|e| e.into_inner());
    let Some((view, started)) = *probe else {
        return;
    };
    if started.elapsed().as_secs() < 3 {
        return;
    }
    let title = (|| -> Option<String> {
        let _frame = env.local_frame(16)?;
        let class = env.find_class("android/webkit/WebView")?;
        let get = env.method_id(class, "getTitle", "()Ljava/lang/String;")?;
        let title = env.call_object(view as JObject, get, &[])?;
        env.get_string(title)
    })();
    if title.as_deref() == Some("PenguDroid JS OK") {
        println!("webview: HTML and JavaScript page probe passed");
    } else if started.elapsed().as_secs() < 15 {
        return;
    } else {
        eprintln!("webview: page probe failed, title={title:?}");
    }
    env.delete_global_ref(view as JObject);
    *probe = None;
}

pub(super) fn prepare(env: &JniEnv, spec: &AppSpec<'_>) -> Option<()> {
    let _frame = env.local_frame(64)?;
    let root = std::env::var_os("ANDROID_ROOT")?;
    let root = std::path::PathBuf::from(root);
    let apk = root.join("product/app/webview/webview.apk");
    if !apk.is_file() {
        return None;
    }
    let globals = env.find_class("android/app/AppGlobals")?;
    let get_app = env.static_method_id(
        globals,
        "getInitialApplication",
        "()Landroid/app/Application;",
    )?;
    let app = env.call_static_object(globals, get_app, &[])?;
    let context = env.find_class("android/content/Context")?;
    let get_pm = env.method_id(
        context,
        "getPackageManager",
        "()Landroid/content/pm/PackageManager;",
    )?;
    let pm = env.call_object(app, get_pm, &[])?;
    let pm_class = env.find_class("android/content/pm/PackageManager")?;
    let parse = env.method_id(
        pm_class,
        "getPackageArchiveInfo",
        "(Ljava/lang/String;I)Landroid/content/pm/PackageInfo;",
    )?;
    // GET_SIGNATURES | GET_META_DATA: the framework collects and verifies the
    // APK certificates; never substitute the hosted app's placeholder signer.
    let info = env.call_object(
        pm,
        parse,
        &[
            JValue {
                l: env.new_string(&apk.display().to_string()),
            },
            JValue { i: 0xc0 },
        ],
    )?;
    if info.is_null() {
        eprintln!("webview: framework rejected provider APK");
        return None;
    }
    let class = env.find_class("android/content/pm/PackageInfo")?;
    let name = env.get_string(env.object_field(
        info,
        env.field_id(class, "packageName", "Ljava/lang/String;")?,
    ))?;
    let version = env.get_string(env.object_field(
        info,
        env.field_id(class, "versionName", "Ljava/lang/String;")?,
    ))?;
    let signatures = env.object_field(
        info,
        env.field_id(class, "signatures", "[Landroid/content/pm/Signature;")?,
    );
    if name != "com.android.webview" || signatures.is_null() || env.array_length(signatures) == 0 {
        eprintln!("webview: unexpected package or missing verified signatures");
        return None;
    }
    println!(
        "webview: framework verified {name} {version}, {} signer(s)",
        env.array_length(signatures)
    );
    let application = env.object_field(
        info,
        env.field_id(
            class,
            "applicationInfo",
            "Landroid/content/pm/ApplicationInfo;",
        )?,
    );
    let ai = env.find_class("android/content/pm/ApplicationInfo")?;
    let libdir = spec.data_dir.join("webview-provider/lib");
    let native = libdir.join("libwebviewchromium.so");
    let metadata = std::fs::metadata(&apk).ok()?;
    let identity = format!(
        "{version}:{}:{:?}",
        metadata.len(),
        metadata.modified().ok()?
    );
    let marker = libdir.join("provider.identity");
    if !native.is_file()
        || std::fs::read_to_string(&marker).ok().as_deref() != Some(identity.as_str())
    {
        let rawdir = spec.data_dir.join("webview-provider/raw");
        crate::apk::extract_prefix(&apk.display().to_string(), "lib/x86_64/", &rawdir).ok()?;
        std::fs::create_dir_all(&libdir).ok()?;
        for entry in std::fs::read_dir(&rawdir).ok()? {
            let entry = entry.ok()?;
            let bytes = std::fs::read(entry.path()).ok()?;
            let bytes = pengudroid_elf::strip_android_versions(&bytes).ok()?;
            publish_file(&libdir.join(entry.file_name()), &bytes)?;
        }
        publish_file(&marker, identity.as_bytes())?;
    }
    if !native.is_file() {
        return None;
    }
    let support = std::fs::read(root.join("lib64/libwebviewchromium_plat_support.so")).ok()?;
    let support = pengudroid_elf::strip_android_versions(&support).ok()?;
    publish_file(&libdir.join("libwebviewchromium_plat_support.so"), &support)?;
    for (field, value) in [
        ("sourceDir", apk.display().to_string()),
        ("publicSourceDir", apk.display().to_string()),
        ("nativeLibraryDir", libdir.display().to_string()),
        (
            "dataDir",
            spec.data_dir.join("webview-provider").display().to_string(),
        ),
        (
            "deviceProtectedDataDir",
            spec.data_dir.join("webview-provider").display().to_string(),
        ),
        (
            "credentialProtectedDataDir",
            spec.data_dir.join("webview-provider").display().to_string(),
        ),
        ("primaryCpuAbi", "x86_64".to_string()),
    ] {
        env.set_object_field(
            application,
            env.field_id(ai, field, "Ljava/lang/String;")?,
            env.new_string(&value),
        );
    }
    let flags = env.field_id(ai, "flags", "I")?;
    env.set_int_field(
        application,
        flags,
        env.int_field(application, flags) | 0x800000 | 4,
    );
    env.set_int_field(application, env.field_id(ai, "uid", "I")?, spec.uid);
    env.set_boolean_field(application, env.field_id(ai, "enabled", "Z")?, true);
    let manifest = crate::apk::read_apk(&apk.display().to_string()).ok()?;
    let provider_spec = AppSpec {
        package: &name,
        services: &manifest.services,
        ..*spec
    };
    install_service_info_replies(env, application, &provider_spec)?;
    let response_class = env.find_class("android/webkit/WebViewProviderResponse")?;
    let ctor = env.method_id(
        response_class,
        "<init>",
        "(Landroid/content/pm/PackageInfo;I)V",
    )?;
    // No shared RELRO is prepared: use the platform's documented fallback.
    let response = env.new_object(response_class, ctor, &[JValue { l: info }, JValue { i: 3 }])?;
    let code = |class: &str, field: &str| -> Option<u32> {
        let class = env.find_class(class)?;
        u32::try_from(env.static_int_field(class, env.static_field_id(class, field, "I")?)).ok()
    };
    let codes = [
        code(
            "android/content/pm/IPackageManager$Stub",
            "TRANSACTION_hasSystemFeature",
        )?,
        code(
            "android/webkit/IWebViewUpdateService$Stub",
            "TRANSACTION_waitForAndGetProvider",
        )?,
        code(
            "android/webkit/IWebViewUpdateService$Stub",
            "TRANSACTION_getCurrentWebViewPackageName",
        )?,
        code(
            "android/webkit/IWebViewUpdateService$Stub",
            "TRANSACTION_getCurrentWebViewPackage",
        )?,
    ];
    let package_bytes = marshal_object(env, info, "android/content/pm/PackageInfo")?;
    let app_bytes = marshal_object(env, application, "android/content/pm/ApplicationInfo")?;
    let response_bytes = marshal_object(env, response, "android/webkit/WebViewProviderResponse")?;
    let publish: unsafe extern "C" fn(
        *const u32,
        *const u8,
        usize,
        *const u8,
        usize,
        *const u8,
        usize,
    ) = unsafe { std::mem::transmute(shim_symbol("__pengudroid_binder_set_webview")?) };
    // Supply the real loader for this package, as for the hosted application's
    // own LoadedApk; keep the framework's provider verification in the path.
    let thread_class = env.find_class("android/app/ActivityThread")?;
    let current = env.static_method_id(
        thread_class,
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
    )?;
    let thread = env.call_static_object(thread_class, current, &[])?;
    let compat_class = env.find_class("android/content/res/CompatibilityInfo")?;
    let compat = env.static_object_field(
        compat_class,
        env.static_field_id(
            compat_class,
            "DEFAULT_COMPATIBILITY_INFO",
            "Landroid/content/res/CompatibilityInfo;",
        )?,
    );
    let get = env.method_id(thread_class, "getPackageInfoNoCheck", "(Landroid/content/pm/ApplicationInfo;Landroid/content/res/CompatibilityInfo;)Landroid/app/LoadedApk;")?;
    let loaded = env.call_object(
        thread,
        get,
        &[JValue { l: application }, JValue { l: compat }],
    )?;
    if !install_class_loader_on(env, loaded, &apk, &libdir) {
        return None;
    }
    let loader_class = env.find_class("android/webkit/WebViewLibraryLoader")?;
    let reserve = env.static_method_id(loader_class, "reserveAddressSpaceInZygote", "()V")?;
    if !env.call_static_void(loader_class, reserve, &[]) {
        return None;
    }
    unsafe {
        publish(
            codes.as_ptr(),
            package_bytes.as_ptr(),
            package_bytes.len(),
            app_bytes.as_ptr(),
            app_bytes.len(),
            response_bytes.as_ptr(),
            response_bytes.len(),
        )
    };
    // API 37 answers SDK feature queries from the bootstrapped shared cache,
    // before IPC. Publish the newly prepared provider there as well.
    let features = env.find_class("android/content/pm/SystemFeaturesCache")?;
    let get = env.static_method_id(
        features,
        "getInstance",
        "()Landroid/content/pm/SystemFeaturesCache;",
    )?;
    let cache = env.call_static_object(features, get, &[])?;
    let versions = env.method_id(features, "getSdkFeatureVersions", "()[I")?;
    let versions = env.call_object(cache, versions, &[])?;
    let index =
        env.static_method_id(pm_class, "maybeGetSdkFeatureIndex", "(Ljava/lang/String;)I")?;
    let index = env.call_static_int(
        pm_class,
        index,
        &[JValue {
            l: env.new_string("android.software.webview"),
        }],
    )?;
    if index < 0 {
        return None;
    }
    let array = env.find_class("java/lang/reflect/Array")?;
    let set = env.static_method_id(array, "setInt", "(Ljava/lang/Object;II)V")?;
    if !env.call_static_void(
        array,
        set,
        &[JValue { l: versions }, JValue { i: index }, JValue { i: 0 }],
    ) {
        return None;
    }
    // ApplicationSharedMemory's feature snapshot is write-once. Java package
    // queries use this cache; leave the native bootstrap snapshot untouched.
    let factory = env.find_class("android/webkit/WebViewFactory")?;
    let supported = env.static_field_id(factory, "sWebViewSupported", "Ljava/lang/Boolean;")?;
    env.set_static_object_field(factory, supported, std::ptr::null_mut());
    println!("webview: verified provider package, loader and update service installed");
    if std::env::var_os("PENGUDROID_WEBVIEW_SINGLE_PROCESS").is_some() {
        // This pinned provider's CommandLine class is R8-renamed WV.iu. Use
        // its real switch API, before browser startup, rather than impersonating
        // Android's isolated renderer service in the browser process.
        if version != "145.0.7632.218" {
            return None;
        }
        let get_provider = env.static_method_id(
            factory,
            "getProvider",
            "()Landroid/webkit/WebViewFactoryProvider;",
        )?;
        let provider = env.call_static_object(factory, get_provider, &[])?;
        if provider.is_null() {
            return None;
        }
        let loaded_class = env.find_class("android/app/LoadedApk")?;
        let loader = env.object_field(
            loaded,
            env.field_id(loaded_class, "mClassLoader", "Ljava/lang/ClassLoader;")?,
        );
        let cl = env.find_class("java/lang/ClassLoader")?;
        let load = env.method_id(cl, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")?;
        let command = env.call_object(
            loader,
            load,
            &[JValue {
                l: env.new_string("WV.iu"),
            }],
        )?;
        let instance =
            env.static_object_field(command, env.static_field_id(command, "d", "LWV/iu;")?);
        // WebView selects its renderer mode with its own switch, independently
        // of content's generic single-process flag (AwBrowserProcess.java).
        let remove = env.method_id(command, "e", "(Ljava/lang/String;)V")?;
        if !env.call_void(
            instance,
            remove,
            &[JValue {
                l: env.new_string("webview-sandboxed-renderer"),
            }],
        ) {
            return None;
        }
        let append = env.method_id(command, "a", "(Ljava/lang/String;Ljava/lang/String;)V")?;
        if !env.call_void(
            instance,
            append,
            &[
                JValue {
                    l: env.new_string("single-process"),
                },
                JValue {
                    l: std::ptr::null_mut(),
                },
            ],
        ) {
            return None;
        }
        println!("webview: Chromium single-process renderer switch installed");
    }
    if std::env::var_os("PENGUDROID_PROBE_WEBVIEW").is_some()
        || std::env::var_os("PENGUDROID_WEBVIEW_PREWARM").is_some()
    {
        let cookies = env.find_class("android/webkit/CookieManager")?;
        let get =
            env.static_method_id(cookies, "getInstance", "()Landroid/webkit/CookieManager;")?;
        let manager = env.call_static_object(cookies, get, &[])?;
        if manager.is_null() {
            return None;
        }
        println!("webview: real CookieManager initialized");
        let view_class = env.find_class("android/webkit/WebView")?;
        let ctor = env.method_id(view_class, "<init>", "(Landroid/content/Context;)V")?;
        let view = env.new_object(view_class, ctor, &[JValue { l: app }])?;
        if std::env::var_os("PENGUDROID_WEBVIEW_SOFTWARE").is_some() {
            let set = env.method_id(view_class, "setLayerType", "(ILandroid/graphics/Paint;)V")?;
            if !env.call_void(
                view,
                set,
                &[
                    JValue { i: 1 },
                    JValue {
                        l: std::ptr::null_mut(),
                    },
                ],
            ) {
                return None;
            }
        }
        let settings =
            env.method_id(view_class, "getSettings", "()Landroid/webkit/WebSettings;")?;
        let settings = env.call_object(view, settings, &[])?;
        let settings_class = env.find_class("android/webkit/WebSettings")?;
        let enable = env.method_id(settings_class, "setJavaScriptEnabled", "(Z)V")?;
        if !env.call_void(settings, enable, &[JValue { z: 1 }]) {
            return None;
        }
        let load = env.method_id(view_class, "loadDataWithBaseURL", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V")?;
        let html = "<!doctype html><title>Loading</title><script>document.title='PenguDroid JS OK'</script><p>PenguDroid WebView</p>";
        if !env.call_void(
            view,
            load,
            &[
                JValue {
                    l: env.new_string("https://pengudroid.invalid/"),
                },
                JValue {
                    l: env.new_string(html),
                },
                JValue {
                    l: env.new_string("text/html"),
                },
                JValue {
                    l: env.new_string("UTF-8"),
                },
                JValue {
                    l: std::ptr::null_mut(),
                },
            ],
        ) {
            return None;
        }
        let view = env.new_global_ref(view)?;
        *PAGE_PROBE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((view as usize, std::time::Instant::now()));
        println!("webview: HTML/JavaScript page probe queued");
    }
    Some(())
}
