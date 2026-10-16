// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "SwiftPMDemo",
    targets: [
        .target(name: "DemoLib"),
        .executableTarget(name: "DemoApp", dependencies: ["DemoLib"]),
    ]
)
