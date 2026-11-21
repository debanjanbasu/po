import Foundation
import LocalAuthentication

@objc(BiometricPlugin)
public class BiometricPlugin: NSObject {
    
    /// Returns "true" if biometrics or passcode are available.
    @objc(canCheck)
    public func canCheck() -> String {
        let context = LAContext()
        var error: NSError?
        let canEvaluate = unsafe context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error)
        return canEvaluate ? "true" : "false"
    }

    /// Prompts for authentication. 
    /// This function blocks the caller, so it MUST be called from a background thread in Rust.
    /// `LAContext.evaluatePolicy` is thread-safe — the system handles UI presentation internally.
    @objc(authenticate:)
    public func authenticate(_ reason: String) -> String {
        let semaphore = DispatchSemaphore(value: 0)
        let result = LockedBox(false)
        
        let context = LAContext()
        context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: reason) { success, _ in
            result.set(success)
            semaphore.signal()
        }
        
        // Bound the wait at 60s (parity with Android's plugin): if the system never invokes the
        // completion (app backgrounded mid-prompt, SpringBoard interruption), unblock the caller
        // instead of leaking a spawn_blocking thread.
        //
        // `invalidate()` is the cancel handle: it stops the pending evaluation, which takes the
        // prompt down and fails the completion with `LAError.appCancel` (the completion then
        // writes into a box nobody reads, which is harmless). Returning without it left the Face
        // ID sheet on screen after Rust had already been told the authentication failed, and a
        // retry stacked a second prompt. Android's plugin cancels at the same deadline
        // (`cancelAuthentication()`).
        if semaphore.wait(timeout: .now() + 60) == .timedOut {
            context.invalidate()
            return "false"
        }
        return result.value ? "true" : "false"
    }
}
