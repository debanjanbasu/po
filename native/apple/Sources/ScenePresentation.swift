import Foundation

/// Thread-safe box for passing a result across concurrency/semaphore boundaries.
final class LockedBox<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var storedValue: T

    init(_ initial: T) {
        storedValue = initial
    }

    var value: T {
        lock.lock()
        defer { lock.unlock() }
        return storedValue
    }

    func set(_ value: T) {
        lock.lock()
        defer { lock.unlock() }
        storedValue = value
    }
}

#if os(iOS)
import UIKit

@MainActor
enum ScenePresentation {
    static func topViewController() -> UIViewController? {
        var controller = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .filter { $0.activationState == .foregroundActive }
            .flatMap(\.windows)
            .first(where: \.isKeyWindow)?
            .rootViewController

        while let presented = controller?.presentedViewController, !presented.isBeingDismissed {
            controller = presented
        }
        guard let controller,
              !controller.isBeingDismissed,
              controller.viewIfLoaded?.window != nil
        else { return nil }
        return controller
    }
}
#endif
