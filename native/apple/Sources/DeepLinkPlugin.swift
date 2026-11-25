import Foundation

#if os(iOS)
import ObjectiveC.runtime
import UIKit
#endif

#if os(macOS)
import AppKit
import CoreServices
#endif

private typealias NotifyFn = @convention(c) () -> Void

private let notifyActivityChangedFn: NotifyFn? = {
    guard let sym = unsafe dlsym(dlopen(nil, RTLD_LAZY), "po_notify_app_activity_changed") else { return nil }
    return unsafe unsafeBitCast(sym, to: NotifyFn.self)
}()

private let notifyBackgroundFn: NotifyFn? = {
    guard let sym = unsafe dlsym(dlopen(nil, RTLD_LAZY), "po_notify_app_background") else { return nil }
    return unsafe unsafeBitCast(sym, to: NotifyFn.self)
}()

private func po_notify_app_activity_changed() {
    notifyActivityChangedFn?()
}

private func po_notify_app_background() {
    notifyBackgroundFn?()
}

#if os(iOS)
/// Keeps the process runnable for a few seconds after backgrounding so the
/// Rust sync queue can flush pending DHT writes before iOS suspends it.
@MainActor
private enum BackgroundFlushRunway {
    static var taskId: UIBackgroundTaskIdentifier = .invalid
    static var generation: UInt64 = 0

    static func begin() {
        end()
        generation &+= 1
        let currentGeneration = generation
        taskId = UIApplication.shared.beginBackgroundTask(withName: "po.sync.flush") {
            Task { @MainActor in
                if generation == currentGeneration { end() }
            }
        }
        guard taskId != .invalid else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 8) {
            if generation == currentGeneration { end() }
        }
    }

    static func end() {
        if taskId != .invalid {
            UIApplication.shared.endBackgroundTask(taskId)
            taskId = .invalid
        }
    }
}
#endif

/// Stores the pending deep link URL received by the app via URL scheme.
/// Thread-safe via a lock.
@objc(DeepLinkPlugin)
public class DeepLinkPlugin: NSObject {
    private static let lock = NSLock()
    private static nonisolated(unsafe) var pendingUrlValue: String = ""

    #if os(iOS) || os(macOS)
    nonisolated(unsafe) private static var lifecycleObserverToken: (any NSObjectProtocol)?
    nonisolated(unsafe) private static var backgroundObserverToken: (any NSObjectProtocol)?
    private static let lifecycleObserverInstalled: Void = {
        installLifecycleObserver()
    }()
    #endif

    #if os(macOS)
    // macOS delivers `po://` opens as kAEGetURL Apple Events, not via any
    // delegate the WKWebView shell installs. Register our own handler and
    // re-assert it around app launch so a default handler installed later
    // cannot displace it.
    nonisolated(unsafe) private static var urlEventObserverTokens: [any NSObjectProtocol] = []
    private static let urlEventHandlerInstalled: Void = {
        DeepLinkURLEventHandler.installOnMainThread()
        for name in [
            NSApplication.willFinishLaunchingNotification,
            NSApplication.didFinishLaunchingNotification,
        ] {
            let token = NotificationCenter.default.addObserver(
                forName: name,
                object: nil,
                queue: nil
            ) { _ in
                DeepLinkURLEventHandler.installOnMainThread()
            }
            unsafe urlEventObserverTokens.append(token)
        }
    }()
    #endif

    @objc public override init() {
        super.init()
        #if os(iOS) || os(macOS)
        _ = Self.lifecycleObserverInstalled
        #endif
        #if os(macOS)
        _ = Self.urlEventHandlerInstalled
        #endif
    }

    /// Called by the app delegate when a URL opens the app.
    @objc public class func setPendingUrl(_ url: String) {
        guard !url.isEmpty else {
            return
        }
        lock.lock()
        unsafe pendingUrlValue = url
        lock.unlock()
        po_notify_app_activity_changed()
    }

    /// Called before UIKit launches, after Tao registers its app delegate.
    @objc(prepareForLaunch)
    public func prepareForLaunch() -> String {
        #if os(iOS)
        return DeepLinkRuntime.installHooks()
            ? "ok"
            : "error: Tao scene configuration is unavailable"
        #else
        return "ok"
        #endif
    }

    /// Returns the pending URL (or empty string if none), then clears it.
    @objc(pendingUrl)
    public func pendingUrl() -> String {
        #if os(macOS)
        _ = Self.urlEventHandlerInstalled
        #endif
        DeepLinkPlugin.lock.lock()
        let url = unsafe DeepLinkPlugin.pendingUrlValue
        unsafe DeepLinkPlugin.pendingUrlValue = ""
        DeepLinkPlugin.lock.unlock()
        return url
    }

    #if os(iOS) || os(macOS)
    private class func installLifecycleObserver() {
        #if os(iOS)
        unsafe lifecycleObserverToken = NotificationCenter.default.addObserver(
            forName: UIScene.didActivateNotification,
            object: nil,
            queue: .main
        ) { _ in
            MainActor.assumeIsolated {
                BackgroundFlushRunway.end()
                po_notify_app_activity_changed()
            }
        }
        unsafe backgroundObserverToken = NotificationCenter.default.addObserver(
            forName: UIScene.didEnterBackgroundNotification,
            object: nil,
            queue: .main
        ) { _ in
            MainActor.assumeIsolated {
                guard !UIApplication.shared.connectedScenes.contains(where: {
                    $0.activationState == .foregroundActive
                        || $0.activationState == .foregroundInactive
                }) else { return }
                BackgroundFlushRunway.begin()
                po_notify_app_background()
            }
        }
        #elseif os(macOS)
        unsafe lifecycleObserverToken = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification,
            object: nil,
            queue: nil
        ) { _ in
            po_notify_app_activity_changed()
        }
        unsafe backgroundObserverToken = NotificationCenter.default.addObserver(
            forName: NSApplication.willResignActiveNotification,
            object: nil,
            queue: nil
        ) { _ in
            po_notify_app_background()
        }
        #endif
    }
    #endif
}

#if os(iOS)
private enum DeepLinkRuntime {
    fileprivate static let applicationConfigurationSelector = NSSelectorFromString("application:configurationForConnectingSceneSession:options:")
    fileprivate static let applicationConfigurationAlias = NSSelectorFromString("po_original_application:configurationForConnectingSceneSession:options:")
    fileprivate static let applicationOpenURLSelector = NSSelectorFromString("application:openURL:options:")
    fileprivate static let applicationOpenURLAlias = NSSelectorFromString("po_original_application:openURL:options:")
    fileprivate static let sceneOpenURLSelector = NSSelectorFromString("scene:openURLContexts:")
    fileprivate static let sceneOpenURLAlias = NSSelectorFromString("po_original_scene:openURLContexts:")
    fileprivate static let sceneWillConnectSelector = NSSelectorFromString("scene:willConnectToSession:options:")
    fileprivate static let sceneWillConnectAlias = NSSelectorFromString("po_original_scene:willConnectToSession:options:")
    fileprivate static let sceneContinueSelector = NSSelectorFromString("scene:continueUserActivity:")
    fileprivate static let sceneContinueAlias = NSSelectorFromString("po_original_scene:continueUserActivity:")

    static func installHooks() -> Bool {
        guard let appDelegate = NSClassFromString("AppDelegate") else {
            return false
        }
        let installed = unsafe swizzle(
            on: appDelegate,
            original: applicationConfigurationSelector,
            alias: applicationConfigurationAlias,
            implementation: unsafe unsafeBitCast(applicationConfigurationHook, to: IMP.self)
        )
        // A URL opened while the app is ALREADY running arrives through the
        // app delegate's legacy `application:openURL:options:` — the scene
        // callbacks are not called for it. Without this hook the URL is
        // swallowed by UIKit and the app never sees it.
        let warmUrlInstalled = unsafe swizzle(
            on: appDelegate,
            original: applicationOpenURLSelector,
            alias: applicationOpenURLAlias,
            implementation: unsafe unsafeBitCast(applicationOpenURLHook, to: IMP.self)
        )
        // UIKit only asks for a scene configuration when it creates a *new*
        // scene session. On every later launch it restores the persisted
        // session and never calls `configurationForConnecting…`, so the scene
        // delegate would keep Tao's unhooked `scene:openURLContexts:` and every
        // warm URL would be dropped. Swizzle the delegate class directly: the
        // class is registered by Tao before the event loop runs, so it exists
        // here, and the hooks are idempotent.
        if let sceneDelegate = NSClassFromString("TaoSceneDelegate") {
            installSceneHooks(on: sceneDelegate)
        }
        // Either hook failing is a real degradation (cold URLs or warm URLs
        // dropped), so report `false` rather than "ok" and name the one that
        // did not take.
        if !installed || !warmUrlInstalled {
            NSLog("DeepLinkRuntime could not install all app-delegate URL handlers")
        }
        return installed && warmUrlInstalled
    }

    static func installSceneHooks(on sceneDelegate: AnyClass) {
        let connected = unsafe swizzle(
            on: sceneDelegate,
            original: sceneWillConnectSelector,
            alias: sceneWillConnectAlias,
            implementation: unsafe unsafeBitCast(sceneWillConnectHook, to: IMP.self)
        )
        let opened = unsafe swizzle(
            on: sceneDelegate,
            original: sceneOpenURLSelector,
            alias: sceneOpenURLAlias,
            implementation: unsafe unsafeBitCast(sceneOpenURLHook, to: IMP.self)
        )
        let continued = unsafe swizzle(
            on: sceneDelegate,
            original: sceneContinueSelector,
            alias: sceneContinueAlias,
            implementation: unsafe unsafeBitCast(sceneContinueHook, to: IMP.self)
        )
        if !connected || !opened || !continued {
            NSLog("DeepLinkRuntime could not install all scene URL handlers")
        }
    }

    private static func swizzle(on targetClass: AnyClass, original: Selector, alias: Selector, implementation: IMP) -> Bool {
        if unsafe class_getInstanceMethod(targetClass, alias) != nil {
            return true
        }
        guard let originalMethod = unsafe class_getInstanceMethod(targetClass, original) else {
            return false
        }
        let typeEncoding = unsafe method_getTypeEncoding(originalMethod)
        guard unsafe class_addMethod(
            targetClass, alias, method_getImplementation(originalMethod), typeEncoding
        ) else { return false }
        unsafe class_replaceMethod(targetClass, original, implementation, typeEncoding)
        return true
    }
}

private typealias ApplicationConfigurationHook = @MainActor @convention(c) (
    AnyObject,
    Selector,
    UIApplication,
    UISceneSession,
    UIScene.ConnectionOptions
) -> UISceneConfiguration

private let applicationConfigurationHook: ApplicationConfigurationHook = { this, _, application, session, options in
    let original = unsafe unsafeBitCast(
        this.method(for: DeepLinkRuntime.applicationConfigurationAlias),
        to: ApplicationConfigurationHook.self
    )
    let configuration = original(
        this, DeepLinkRuntime.applicationConfigurationAlias, application, session, options
    )
    if let sceneDelegate = configuration.delegateClass {
        DeepLinkRuntime.installSceneHooks(on: sceneDelegate)
    } else {
        NSLog("DeepLinkRuntime received a scene configuration without a delegate")
    }
    return configuration
}

private typealias ApplicationOpenURLHook = @MainActor @convention(c) (
    AnyObject,
    Selector,
    UIApplication,
    URL,
    [AnyHashable: Any]
) -> Bool

private let applicationOpenURLHook: ApplicationOpenURLHook = { this, _, application, url, options in
    DeepLinkPlugin.setPendingUrl(url.absoluteString)
    let original = unsafe unsafeBitCast(
        this.method(for: DeepLinkRuntime.applicationOpenURLAlias),
        to: ApplicationOpenURLHook.self
    )
    return original(this, DeepLinkRuntime.applicationOpenURLAlias, application, url, options)
}

private typealias SceneOpenURLHook = @MainActor @convention(c) (
    AnyObject,
    Selector,
    UIScene,
    NSSet
) -> Void

private let sceneOpenURLHook: SceneOpenURLHook = { this, _, scene, urlContexts in
    if let context = urlContexts.allObjects.first as? UIOpenURLContext {
        DeepLinkPlugin.setPendingUrl(context.url.absoluteString)
    }
    let original = unsafe unsafeBitCast(this.method(for: DeepLinkRuntime.sceneOpenURLAlias), to: SceneOpenURLHook.self)
    original(this, DeepLinkRuntime.sceneOpenURLAlias, scene, urlContexts)
}

private typealias SceneWillConnectHook = @MainActor @convention(c) (
    AnyObject,
    Selector,
    UIScene,
    UISceneSession,
    UIScene.ConnectionOptions
) -> Void

private let sceneWillConnectHook: SceneWillConnectHook = { this, _, scene, session, options in
    if let url = options.urlContexts.first?.url
        ?? options.userActivities.compactMap(\.webpageURL).first
    {
        DeepLinkPlugin.setPendingUrl(url.absoluteString)
    }
    let original = unsafe unsafeBitCast(this.method(for: DeepLinkRuntime.sceneWillConnectAlias), to: SceneWillConnectHook.self)
    original(this, DeepLinkRuntime.sceneWillConnectAlias, scene, session, options)
}

private typealias SceneContinueHook = @MainActor @convention(c) (
    AnyObject,
    Selector,
    UIScene,
    NSUserActivity
) -> Void

private let sceneContinueHook: SceneContinueHook = { this, _, scene, activity in
    if let url = activity.webpageURL {
        DeepLinkPlugin.setPendingUrl(url.absoluteString)
    }
    let original = unsafe unsafeBitCast(this.method(for: DeepLinkRuntime.sceneContinueAlias), to: SceneContinueHook.self)
    original(this, DeepLinkRuntime.sceneContinueAlias, scene, activity)
}
#endif

#if os(macOS)
/// Receives `kAEGetURL` Apple Events and stashes the URL in
/// `DeepLinkPlugin`. Registration happens on the main thread; the handler
/// object lives for the whole process.
private final class DeepLinkURLEventHandler: NSObject {
    nonisolated(unsafe) static let shared = DeepLinkURLEventHandler()

    static func installOnMainThread() {
        if Thread.isMainThread {
            registerShared()
        } else {
            DispatchQueue.main.async {
                registerShared()
            }
        }
    }

    private static func registerShared() {
        unsafe shared.register()
    }

    func register() {
        NSAppleEventManager.shared().setEventHandler(
            self,
            andSelector: #selector(handleGetURL(_:withReplyEvent:)),
            forEventClass: AEEventClass(kInternetEventClass),
            andEventID: AEEventID(kAEGetURL)
        )
    }

    @objc func handleGetURL(
        _ event: NSAppleEventDescriptor,
        withReplyEvent _: NSAppleEventDescriptor
    ) {
        guard let url = event.paramDescriptor(forKeyword: AEKeyword(keyDirectObject))?.stringValue
        else {
            NSLog("DeepLinkURLEventHandler received kAEGetURL without URL")
            return
        }
        DeepLinkPlugin.setPendingUrl(url)
    }
}
#endif
