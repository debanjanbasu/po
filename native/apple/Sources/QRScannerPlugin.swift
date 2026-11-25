import AVFoundation
import Foundation
import Vision

#if os(macOS)
import AppKit
#endif
#if os(iOS)
import UIKit
import VisionKit
#endif

@objc(QRScannerPlugin)
public final class QRScannerPlugin: NSObject {
    private static let lock = NSLock()
    private static nonisolated(unsafe) var activeScanner: QRCaptureController?

    private static func setActiveScanner(_ scanner: QRCaptureController?) {
        lock.lock()
        unsafe activeScanner = scanner
        lock.unlock()
    }

    /// Returns "true" if the device has a camera that supports video and
    /// access is not denied. Permission not yet determined counts as
    /// scannable, since we request access at scan time.
    @objc(canScan)
    public func canScan() -> String {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .denied, .restricted:
            return "false"
        default:
            return AVCaptureDevice.default(for: .video) != nil ? "true" : "false"
        }
    }

    /// Synchronous QR scan callable from any thread.
    /// Dispatches to the main thread for camera UI and blocks until complete.
    @objc(scanSync)
    public func scanSync() -> String {
        // Blocking the main thread would deadlock: the scanner UI itself must
        // run there. Rust callers invoke this from a blocking worker thread.
        guard !Thread.isMainThread else { return "" }
        let resultBox = LockedBox("")
        let semaphore = DispatchSemaphore(value: 0)
        // Only what this call installed: a scan started after this one timed out
        // must not be torn down by this call's cleanup.
        let scannerForThisCall = LockedBox<QRCaptureController?>(nil)
        DispatchQueue.main.async {
            Self.lock.lock()
            let priorScanner = unsafe Self.activeScanner
            unsafe Self.activeScanner = nil
            Self.lock.unlock()
            priorScanner?.cancel()

            let scanner = QRCaptureController { code in
                Self.setActiveScanner(nil)
                resultBox.set(code)
                semaphore.signal()
            } onCancel: {
                Self.setActiveScanner(nil)
                semaphore.signal()
            }
            Self.setActiveScanner(scanner)
            scannerForThisCall.set(scanner)
            Task { @MainActor in
                await scanner.present()
            }
        }
        // Bounded like every other bridge: `BiometricPlugin.authenticate` and
        // `SharePlugin.pickFile` wait 60s, and the headless Linux/Windows loops
        // use the same budget (`config::qr::SCAN_TIMEOUT_SECS`). The scan ends
        // only when the controller calls back, and the VisionKit path is the one
        // exit that depends on a UIKit completion firing — if presenting is
        // refused or interrupted, nothing calls `finish(with:)`, and an
        // unbounded wait would park this Rust worker thread and keep the capture
        // session alive for the life of the process.
        if semaphore.wait(timeout: .now() + 60) == .timedOut {
            // The controller's own completion clears `activeScanner`, dismisses
            // the UI and stops the camera; the extra signal lands in a semaphore
            // nobody is waiting on.
            if let abandoned = scannerForThisCall.value {
                DispatchQueue.main.async { abandoned.cancel() }
            }
            return ""
        }
        return resultBox.value
    }

    /// Cancels any in-flight scan, dismissing its UI and releasing the
    /// camera. Safe to call from any thread; returns "true" if a scan was
    /// active.
    @objc(cancelScan)
    public func cancelScan() -> String {
        Self.lock.lock()
        let scanner = unsafe Self.activeScanner
        Self.lock.unlock()
        guard let scanner = scanner else { return "false" }
        DispatchQueue.main.async {
            scanner.cancel()
        }
        return "true"
    }

}

// MARK: - Frame Scanner (capture-queue confined)

/// Nonisolated sample-buffer delegate: AVFoundation delivers frames on the
/// dedicated scan queue, so this type must NOT be MainActor-isolated — a
/// @preconcurrency conformance on a @MainActor class traps
/// (dispatch_assert_queue_fail / SIGTRAP) in the @objc thunk's executor
/// check as soon as the first frame arrives. All state here is confined to
/// the delegate queue.
private final class QRFrameScanner: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private let requestHandler = VNSequenceRequestHandler()
    private let barcodeRequest: VNDetectBarcodesRequest = {
        let request = VNDetectBarcodesRequest()
        request.symbologies = [.qr]
        return request
    }()
    private var isDone = false
    private var lastPass: UInt64 = 0
    /// ~10 Hz is plenty for QR detection; running Vision on every frame just
    /// burns CPU/battery with no perceptible latency win.
    private let minPassInterval: UInt64 = 100_000_000
    private let onCode: @Sendable (String) -> Void

    init(onCode: @escaping @Sendable (String) -> Void) {
        self.onCode = onCode
    }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        guard !isDone, let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        guard now &- lastPass >= minPassInterval else { return }
        lastPass = now
        do {
            try requestHandler.perform([barcodeRequest], on: pixelBuffer)
            if let result = barcodeRequest.results?.first?.payloadStringValue, !result.isEmpty {
                isDone = true
                onCode(result)
            }
        } catch {}
    }
}

// MARK: - Modernized QR Capture Controller

@MainActor
private final class QRCaptureController: NSObject {
    private var session: AVCaptureSession?
    private let onResult: (String) -> Void
    private let onCancel: () -> Void

    private let scanQueue = DispatchQueue(label: "com.debanjanbasu.po.qrscanner.vision", qos: .userInitiated)
    private let sessionQueue = DispatchQueue(label: "com.debanjanbasu.po.qrscanner.capture", qos: .userInitiated)
    private var frameScanner: QRFrameScanner?
#if os(iOS)
    private var dataScanner: DataScannerViewController?
    private var scannerHost: UIViewController?
#endif

    private var isFinished = false

#if os(macOS)
    private var previewOverlayView: NSView?
#endif
#if os(iOS)
    private var previewOverlayView: UIView?
#endif
    private var previewLayer: AVCaptureVideoPreviewLayer?

    init(onResult: @escaping (String) -> Void, onCancel: @escaping () -> Void) {
        self.onResult = onResult
        self.onCancel = onCancel
        super.init()
    }

    func present() async {
        guard !isFinished else { return }
        let status = AVCaptureDevice.authorizationStatus(for: .video)
        switch status {
        case .authorized:
            await beginScanning()
        case .notDetermined:
            let granted = await AVCaptureDevice.requestAccess(for: .video)
            if granted {
                await beginScanning()
            } else {
                finish(with: nil)
            }
        default:
            finish(with: nil)
        }
    }

    /// Prefers the system scanner UI (VisionKit) on iOS — highlighting,
    /// guidance, pinch-to-zoom for free. Falls back to the custom
    /// AVFoundation + Vision pipeline on macOS or when the scanner is
    /// unavailable (e.g. Screen Time camera restrictions).
    private func beginScanning() async {
        guard !isFinished else { return }
#if os(iOS)
        if DataScannerViewController.isSupported, DataScannerViewController.isAvailable {
            presentDataScanner()
            return
        }
#endif
        await startCapture()
    }

    /// External (Rust-side) cancellation: stops capture, dismisses UI, and
    /// reports cancel. Must be called on the main thread.
    func cancel() {
        finish(with: nil)
    }

    private func finish(with value: String?) {
        guard !isFinished else { return }
        isFinished = true

        let localSession = session
        session = nil

        nonisolated(unsafe) let localSessionCopy = localSession
        sessionQueue.async {
            unsafe localSessionCopy?.stopRunning()
        }

        hidePreview()
#if os(iOS)
        if let scanner = dataScanner {
            scanner.stopScanning()
            dataScanner = nil
        }
        if let host = scannerHost {
            host.presentingViewController?.dismiss(animated: true)
            scannerHost = nil
        }
#endif

        if let value = value, !value.isEmpty {
            onResult(value)
        } else {
            onCancel()
        }
    }

    private func startCapture() async {
        guard !isFinished else { return }
        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device) else {
            finish(with: nil)
            return
        }

        let session = AVCaptureSession()
        session.beginConfiguration()

        // 720p decodes QR codes reliably; higher resolutions only make each
        // Vision pass slower and burn battery.
        if session.canSetSessionPreset(.hd1280x720) {
            session.sessionPreset = .hd1280x720
        }

        guard session.canAddInput(input) else {
            session.commitConfiguration()
            finish(with: nil)
            return
        }
        session.addInput(input)

        let output = AVCaptureVideoDataOutput()
        output.alwaysDiscardsLateVideoFrames = true
        // Sensor-native format: Vision consumes it directly, skipping a
        // per-frame BGRA color conversion.
        output.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange]

        guard session.canAddOutput(output) else {
            session.commitConfiguration()
            finish(with: nil)
            return
        }
        session.addOutput(output)
        let scanner = QRFrameScanner { [weak self] code in
            Task { @MainActor in
                self?.finish(with: code)
            }
        }
        frameScanner = scanner
        output.setSampleBufferDelegate(scanner, queue: scanQueue)

        session.commitConfiguration()
        self.session = session

        guard showPreview(session: session) else {
            NSLog("QR scan cancelled: no foreground window is available")
            finish(with: nil)
            return
        }

        nonisolated(unsafe) let captureSession = session
        sessionQueue.async {
            unsafe captureSession.startRunning()
        }
    }

    private func showPreview(session: AVCaptureSession) -> Bool {
#if os(macOS)
        guard let hostView = (NSApp.keyWindow ?? NSApp.mainWindow)?.contentView else { return false }
#else
        guard let hostView = ScenePresentation.topViewController()?.view else { return false }
#endif

        let overlay: PlatformView = {
#if os(macOS)
            let v = NSView(frame: hostView.bounds)
            v.autoresizingMask = [.width, .height]
            v.wantsLayer = true
            v.layer?.backgroundColor = NSColor.black.cgColor
            return v
#else
            let v = UIView(frame: hostView.bounds)
            v.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            v.backgroundColor = .black
            return v
#endif
        }()

        let previewLayer = AVCaptureVideoPreviewLayer(session: session)
        previewLayer.frame = overlay.bounds
        previewLayer.videoGravity = .resizeAspectFill
#if os(macOS)
        overlay.layer?.addSublayer(previewLayer)
#else
        overlay.layer.addSublayer(previewLayer)
#endif

        setupUI(on: overlay)
        hostView.addSubview(overlay)

        self.previewOverlayView = overlay
        self.previewLayer = previewLayer
        return true
    }

    private func setupUI(on overlay: PlatformView) {
#if os(macOS)
        let closeButton = NSButton(title: "Cancel", target: self, action: #selector(cancelFromPreview))
        closeButton.bezelStyle = .rounded
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        overlay.addSubview(closeButton)

        let hint = NSTextField(labelWithString: "Point your camera at the QR code")
        hint.textColor = .white
        hint.translatesAutoresizingMaskIntoConstraints = false
        overlay.addSubview(hint)
#else
        let closeButton = UIButton(type: .system)
        closeButton.setTitle("Cancel", for: .normal)
        closeButton.tintColor = .white
        closeButton.backgroundColor = UIColor.black.withAlphaComponent(0.5)
        closeButton.layer.cornerRadius = 10
        closeButton.addTarget(self, action: #selector(cancelFromPreview), for: .touchUpInside)
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        overlay.addSubview(closeButton)

        let hint = UILabel()
        hint.text = "Point your camera at the QR code"
        hint.textColor = .white
        hint.translatesAutoresizingMaskIntoConstraints = false
        overlay.addSubview(hint)
#endif

        #if os(macOS)
        let topAnchor = overlay.topAnchor
        let trailingAnchor = overlay.trailingAnchor
        let bottomAnchor = overlay.bottomAnchor
        #else
        let topAnchor = overlay.safeAreaLayoutGuide.topAnchor
        let trailingAnchor = overlay.safeAreaLayoutGuide.trailingAnchor
        let bottomAnchor = overlay.safeAreaLayoutGuide.bottomAnchor
        #endif
        NSLayoutConstraint.activate([
            closeButton.topAnchor.constraint(equalTo: topAnchor, constant: 16),
            closeButton.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
            hint.centerXAnchor.constraint(equalTo: overlay.centerXAnchor),
            hint.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -24)
        ])
    }

    private func hidePreview() {
        previewLayer?.removeFromSuperlayer()
        previewOverlayView?.removeFromSuperview()
        previewOverlayView = nil
        previewLayer = nil
    }

    @objc private func cancelFromPreview() {
        finish(with: nil)
    }

}

#if os(iOS)
// MARK: - VisionKit system scanner (iOS)

@MainActor
private final class ScannerHostViewController: UIViewController {
    var onDismiss: (() -> Void)?

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isBeingDismissed || parent == nil {
            onDismiss?()
        }
    }
}

extension QRCaptureController: DataScannerViewControllerDelegate {
    fileprivate func presentDataScanner() {
        // `topViewController()` already walks past settled presentations, so a
        // non-nil `presentedViewController` here means one is in flight; UIKit
        // refuses the present in that case and the completion below never runs.
        // `SharePlugin.pickFile` takes the same guard.
        guard let presenter = ScenePresentation.topViewController(),
            presenter.presentedViewController == nil
        else {
            NSLog("QR scan cancelled: no presenter is free to present the scanner")
            finish(with: nil)
            return
        }

        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isGuidanceEnabled: true,
            isHighlightingEnabled: true
        )
        scanner.delegate = self
        dataScanner = scanner

        // Embed the scanner in a plain host controller so the Cancel button
        // is a sibling above VisionKit's view; buttons placed inside
        // `overlayContainerView` never receive touches.
        let host = ScannerHostViewController()
        host.onDismiss = { [weak self] in
            guard let self, !self.isFinished else { return }
            self.finish(with: nil)
        }
        host.view.backgroundColor = .black
        host.modalPresentationStyle = .fullScreen
        host.addChild(scanner)
        host.view.addSubview(scanner.view)
        scanner.view.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            scanner.view.topAnchor.constraint(equalTo: host.view.topAnchor),
            scanner.view.bottomAnchor.constraint(equalTo: host.view.bottomAnchor),
            scanner.view.leadingAnchor.constraint(equalTo: host.view.leadingAnchor),
            scanner.view.trailingAnchor.constraint(equalTo: host.view.trailingAnchor),
        ])
        scanner.didMove(toParent: host)
        addCancelButton(to: host.view)
        scannerHost = host

        presenter.present(host, animated: true) { [weak self, weak scanner] in
            guard let self, let scanner, !self.isFinished else { return }
            do {
                try scanner.startScanning()
            } catch {
                self.finish(with: nil)
            }
        }
    }

    private func addCancelButton(to container: UIView) {
        var config = UIButton.Configuration.filled()
        config.title = "Cancel"
        config.baseForegroundColor = .white
        config.baseBackgroundColor = UIColor.black.withAlphaComponent(0.5)
        config.cornerStyle = .large
        config.contentInsets = NSDirectionalEdgeInsets(top: 10, leading: 16, bottom: 10, trailing: 16)
        let closeButton = UIButton(configuration: config)
        closeButton.addTarget(self, action: #selector(cancelFromPreview), for: .touchUpInside)
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(closeButton)
        NSLayoutConstraint.activate([
            closeButton.topAnchor.constraint(equalTo: container.safeAreaLayoutGuide.topAnchor, constant: 16),
            closeButton.trailingAnchor.constraint(equalTo: container.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            closeButton.heightAnchor.constraint(greaterThanOrEqualToConstant: 44),
            closeButton.widthAnchor.constraint(greaterThanOrEqualToConstant: 44),
        ])
    }

    func dataScanner(
        _ dataScanner: DataScannerViewController,
        didAdd addedItems: [RecognizedItem],
        allItems: [RecognizedItem]
    ) {
        let payload = addedItems.lazy
            .compactMap { item -> String? in
                if case let .barcode(barcode) = item { return barcode.payloadStringValue }
                return nil
            }
            .first { !$0.isEmpty }
        if let payload {
            finish(with: payload)
        }
    }

    func dataScanner(
        _ dataScanner: DataScannerViewController,
        becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable
    ) {
        finish(with: nil)
    }
}
#endif

#if os(macOS)
typealias PlatformView = NSView
#else
typealias PlatformView = UIView
#endif
