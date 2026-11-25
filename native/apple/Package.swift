// swift-tools-version:6.4

import PackageDescription

let package = Package(
    name: "NativePlugins",
    platforms: [
        .iOS("27.0"),
        .macOS("27.0"),
    ],
    products: [
        .library(
            name: "BiometricPlugin",
            type: .static,
            targets: ["NativePlugins"]
        ),
        .library(
            name: "QRScannerPlugin",
            type: .static,
            targets: ["NativePlugins"]
        ),
        .library(
            name: "DeepLinkPlugin",
            type: .static,
            targets: ["NativePlugins"]
        ),
        .library(
            name: "SharePlugin",
            type: .static,
            targets: ["NativePlugins"]
        ),
    ],
    dependencies: [],
    targets: [
        .target(
            name: "NativePlugins",
            path: "Sources",
            swiftSettings: [
                // Swift 7 defaults, opted into early (bleeding-edge policy).
                .enableUpcomingFeature("ExistentialAny"),
                .enableUpcomingFeature("MemberImportVisibility"),
                .enableUpcomingFeature("InferIsolatedConformances"),
                .enableUpcomingFeature("NonisolatedNonsendingByDefault"),
                .strictMemorySafety(),
            ],
            linkerSettings: [
                .linkedFramework("LocalAuthentication"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("Foundation"),
                .linkedFramework("Vision"),
                .linkedFramework("VisionKit", .when(platforms: [.iOS])),
            ]
        )
    ]
)
