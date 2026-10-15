// swift-tools-version: 6.2
import PackageDescription

// Private application component. No license grant for application code is implied.
let package = Package(
  name: "SmithMLXWorker",
  platforms: [.macOS(.v15)],
  products: [
    .executable(name: "smith-mlx-worker", targets: ["SmithMLXWorker"]),
    .executable(name: "smith-mlx-fixture", targets: ["SmithMLXFixture"]),
  ],
  dependencies: [
    .package(url: "https://github.com/ml-explore/mlx-swift", exact: "0.31.4"),
    .package(url: "https://github.com/ml-explore/mlx-swift-lm", exact: "3.31.4"),
    .package(url: "https://github.com/huggingface/swift-transformers", exact: "1.3.5"),
    // Newer Collections adds a toolchain-only Swift Span compatibility dylib on macOS 15.
    .package(url: "https://github.com/apple/swift-collections.git", exact: "1.1.4"),
  ],
  targets: [
    .target(name: "WorkerCore"),
    .executableTarget(
      name: "SmithMLXWorker",
      dependencies: [
        "WorkerCore",
        .product(name: "MLX", package: "mlx-swift"),
        .product(name: "MLXLLM", package: "mlx-swift-lm"),
        .product(name: "MLXLMCommon", package: "mlx-swift-lm"),
        .product(name: "Tokenizers", package: "swift-transformers"),
        .product(name: "Hub", package: "swift-transformers"),
      ]),
    .executableTarget(
      name: "SmithMLXFixture",
      dependencies: [
        "WorkerCore",
        .product(name: "MLX", package: "mlx-swift"),
        .product(name: "MLXNN", package: "mlx-swift"),
        .product(name: "MLXLLM", package: "mlx-swift-lm"),
      ]),
    .testTarget(name: "WorkerCoreTests", dependencies: ["WorkerCore"], resources: [.copy("Fixtures")]),
  ]
)
