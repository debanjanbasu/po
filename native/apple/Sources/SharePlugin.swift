import Foundation

#if os(iOS)
import UIKit
import UniformTypeIdentifiers
#elseif os(macOS)
import AppKit
#endif

#if os(iOS)
@MainActor
private final class DocumentPickerDelegate: NSObject, @MainActor UIDocumentPickerDelegate, @MainActor UIAdaptivePresentationControllerDelegate {
    private static var activeDelegate: DocumentPickerDelegate?
    weak var picker: UIDocumentPickerViewController?
    private var completion: ((String) -> Void)?

    init(completion: @escaping (String) -> Void) {
        self.completion = completion
        super.init()
        if let prior = Self.activeDelegate {
            prior.picker?.delegate = nil
            prior.picker?.dismiss(animated: false)
            prior.finish("")
        }
        Self.activeDelegate = self
    }

    private func finish(_ result: String) {
        let callback = completion
        completion = nil
        picker = nil
        if Self.activeDelegate === self {
            Self.activeDelegate = nil
        }
        callback?(result)
    }

    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        controller.delegate = nil
        if let path = urls.first?.path, !path.isEmpty {
            finish(path)
        } else {
            finish("error:no_url")
        }
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        controller.delegate = nil
        finish("")
    }

    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
        finish("")
    }
}
#endif

#if os(macOS)
/// Retains the active `NSSharingServicePicker` while its menu is on screen.
/// AppKit does not retain the picker during menu tracking, so without a
/// strong reference the menu would deallocate mid-interaction. Released
/// when the user chooses a service or dismisses the menu.
@MainActor
private final class SharePickerLifecycle: NSObject, @MainActor NSSharingServicePickerDelegate {
    static let shared = SharePickerLifecycle()
    private var activePicker: NSSharingServicePicker?

    func begin(_ picker: NSSharingServicePicker) {
        activePicker = picker
    }

    func sharingServicePicker(
        _ sharingServicePicker: NSSharingServicePicker,
        didChoose service: NSSharingService?
    ) {
        sharingServicePicker.delegate = nil
        activePicker = nil
    }
}
#endif

@objc(SharePlugin)
public final class SharePlugin: NSObject {
    private func runOnMainThreadSync(
        timeout: TimeInterval = 2.0,
        _ work: @MainActor @Sendable @escaping () -> String
    ) -> String {
        if Thread.isMainThread {
            return MainActor.assumeIsolated { work() }
        }
        let semaphore = DispatchSemaphore(value: 0)
        let result = LockedBox("")
        // A `DispatchWorkItem` so the timeout can actually cancel the work: a
        // plain `async` block still runs after this returned, so the caller would
        // be told the action failed while the pasteboard was written or the share
        // sheet opened a moment later. A queued, unstarted item is skipped.
        //
        // The block is `@Sendable` and unisolated because that is what
        // `DispatchWorkItem`'s block parameter is; `work` keeps its `@MainActor`
        // isolation and `LockedBox` carries the result back across the hop.
        let task = DispatchWorkItem { @Sendable in
            MainActor.assumeIsolated {
                result.set(work())
            }
            semaphore.signal()
        }
        DispatchQueue.main.async(execute: task)
        if semaphore.wait(timeout: .now() + timeout) == .timedOut {
            task.cancel()
            // Distinct from "false": the Rust side reads this as a call that never
            // started, not as the platform reporting a failure.
            return "error:timeout"
        }
        return result.value
    }

    #if os(iOS)
    @MainActor
    private static func presentActivitySheet(
        on presenter: UIViewController,
        items: [Any],
        completion: UIActivityViewController.CompletionWithItemsHandler? = nil
    ) {
        let sheet = UIActivityViewController(activityItems: items, applicationActivities: nil)
        sheet.completionWithItemsHandler = completion
        if let popover = sheet.popoverPresentationController {
            popover.sourceView = presenter.view
            popover.sourceRect = CGRect(
                x: presenter.view.bounds.midX,
                y: presenter.view.bounds.midY,
                width: 1,
                height: 1
            )
            popover.permittedArrowDirections = []
        }
        presenter.present(sheet, animated: true)
    }
    #endif

    /// Shares text, optionally with an embedded image.
    ///
    /// If the input starts with `{"text":`, it is parsed as JSON with
    /// `text` and optional `image` (base64) fields. Otherwise the
    /// entire string is shared as plain text.
    @objc(shareText:)
    public func shareText(_ input: String) -> String {
        let payload = input.hasPrefix("{\"text\":")
            ? input.data(using: .utf8).flatMap { data in
                (try? JSONSerialization.jsonObject(with: data)) as? [String: String]
            }
            : nil
        let textItem = payload?["text"] ?? input
        let imageData = payload?["image"].flatMap { Data(base64Encoded: $0) }
#if os(iOS)
        return runOnMainThreadSync {
            guard let presenter = ScenePresentation.topViewController() else {
                return "false"
            }
            var items: [Any] = [textItem]
            if let imageData, let image = UIImage(data: imageData) {
                items.append(image)
            }

            Self.presentActivitySheet(on: presenter, items: items)
            return "true"
        }
#else
        return runOnMainThreadSync {
            guard let window = NSApp.keyWindow ?? NSApp.mainWindow ?? NSApp.windows.first,
                  let anchorView = window.contentView
            else {
                return "false"
            }
            var items: [Any] = [textItem]
            if let imageData, let image = NSImage(data: imageData) {
                items.append(image)
            }

            let picker = NSSharingServicePicker(items: items)
            picker.delegate = SharePickerLifecycle.shared
            SharePickerLifecycle.shared.begin(picker)
            // Defer show() one run-loop turn: menu tracking nests the run
            // loop, and shareText must return once presentation is initiated
            // (same contract as the iOS share sheet above).
            DispatchQueue.main.async { @MainActor in
                picker.show(relativeTo: .zero, of: anchorView, preferredEdge: .minY)
            }
            return "true"
        }
#endif
    }

    @objc(copyText:)
    public func copyText(_ text: String) -> String {
#if os(iOS)
        return runOnMainThreadSync {
            UIPasteboard.general.string = text
            return "true"
        }
#elseif os(macOS)
        return runOnMainThreadSync {
            let pasteboard = NSPasteboard.general
            pasteboard.clearContents()
            return pasteboard.setString(text, forType: .string) ? "true" : "false"
        }
#else
        let _ = text
        return "false"
#endif
    }

    /// Shares a file via the native share sheet.
    ///
    /// Input is JSON: `{"filename":"name.ext","data":"<base64>"}`.
    /// Writes to a temp directory and presents the share sheet with the file URL.
    @objc(shareFile:)
    public func shareFile(_ input: String) -> String {
        guard let jsonData = input.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: jsonData) as? [String: String],
              let filename = json["filename"],
              let b64 = json["data"],
              let fileData = Data(base64Encoded: b64)
        else {
            return "false"
        }
        guard !filename.isEmpty,
              filename != ".",
              filename != "..",
              !filename.contains("/"),
              !filename.contains("\\"),
              !filename.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })
        else {
            return "false"
        }

        let exportsRoot = FileManager.default.temporaryDirectory
            .appendingPathComponent("po_exports", isDirectory: true)
        if let existing = try? FileManager.default.contentsOfDirectory(
            at: exportsRoot,
            includingPropertiesForKeys: [.creationDateKey, .contentModificationDateKey],
            options: [.skipsHiddenFiles]
        ) {
            let cutoff = Date().addingTimeInterval(-3600)
            for entry in existing {
                let values = try? entry.resourceValues(forKeys: [.creationDateKey, .contentModificationDateKey])
                if let timestamp = values?.contentModificationDate ?? values?.creationDate,
                   timestamp < cutoff
                {
                    try? FileManager.default.removeItem(at: entry)
                }
            }
        }

        let exportDirectory = exportsRoot.appendingPathComponent(UUID().uuidString, isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: exportDirectory, withIntermediateDirectories: true)
        } catch {
            return "false"
        }
        let fileURL = exportDirectory.appendingPathComponent(filename)
        do {
            try fileData.write(to: fileURL)
        } catch {
            try? FileManager.default.removeItem(at: exportDirectory)
            return "false"
        }
#if os(iOS)
        return runOnMainThreadSync {
            guard let presenter = ScenePresentation.topViewController() else {
                try? FileManager.default.removeItem(at: exportDirectory)
                return "false"
            }

            Self.presentActivitySheet(on: presenter, items: [fileURL]) { _, _, _, _ in
                try? FileManager.default.removeItem(at: exportDirectory)
            }
            return "true"
        }
#elseif os(macOS)
        return runOnMainThreadSync {
            let panel = NSSavePanel()
            panel.nameFieldStringValue = filename
            panel.canCreateDirectories = true
            panel.isExtensionHidden = false
            let handler: (NSApplication.ModalResponse) -> Void = { response in
                defer { try? FileManager.default.removeItem(at: exportDirectory) }
                guard response == .OK, let dest = panel.url else { return }
                let staged = dest.deletingLastPathComponent()
                    .appendingPathComponent(".\(dest.lastPathComponent).\(UUID().uuidString).staged")
                do {
                    try FileManager.default.copyItem(at: fileURL, to: staged)
                    if FileManager.default.fileExists(atPath: dest.path) {
                        _ = try FileManager.default.replaceItemAt(
                            dest,
                            withItemAt: staged,
                            backupItemName: nil,
                            options: .usingNewMetadataOnly
                        )
                    } else {
                        try FileManager.default.moveItem(at: staged, to: dest)
                    }
                } catch {
                    NSLog("SharePlugin: failed to save exported file: \(error.localizedDescription)")
                    if FileManager.default.fileExists(atPath: staged.path) {
                        try? FileManager.default.removeItem(at: staged)
                    }
                }
            }
            if let window = NSApp.keyWindow ?? NSApp.mainWindow ?? NSApp.windows.first {
                panel.beginSheetModal(for: window, completionHandler: handler)
            } else {
                panel.begin(completionHandler: handler)
            }
            return "true"
        }
#else
        try? FileManager.default.removeItem(at: exportDirectory)
        return "false"
#endif
    }
    @objc(pickFile)
    public func pickFile() -> String {
#if os(iOS)
        guard !Thread.isMainThread else { return "error:main_thread" }
        let semaphore = DispatchSemaphore(value: 0)
        let result = LockedBox("")
        DispatchQueue.main.async { @MainActor in
            guard let presenter = ScenePresentation.topViewController(),
                  presenter.presentedViewController == nil
            else {
                result.set("error:no_presenter")
                semaphore.signal()
                return
            }
            let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item, .data], asCopy: true)
            picker.allowsMultipleSelection = false
            let delegate = DocumentPickerDelegate { picked in
                result.set(picked)
                semaphore.signal()
            }
            delegate.picker = picker
            picker.delegate = delegate
            picker.presentationController?.delegate = delegate
            presenter.present(picker, animated: true)
        }
        if semaphore.wait(timeout: .now() + 60) == .timedOut {
            return "error:timeout"
        }
        return result.value
#else
        return "error:unsupported"
#endif
    }
}
