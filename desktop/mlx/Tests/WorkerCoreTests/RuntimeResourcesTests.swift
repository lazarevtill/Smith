import Darwin
import Foundation
import XCTest

@testable import WorkerCore

final class RuntimeResourcesTests: XCTestCase {
  private func fixture() throws -> URL {
    let temp = FileManager.default.temporaryDirectory.path
    guard let resolved = realpath(temp, nil) else { throw WorkerError.invalid }
    defer { free(resolved) }
    let root = URL(fileURLWithPath: String(cString: resolved))
      .appendingPathComponent("smith-mlx-resources-" + UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    for path in RuntimeResources.required {
      let file = root.appendingPathComponent(path)
      try FileManager.default.createDirectory(
        at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
      try Data((path.hasSuffix(".metallib") ? "MTLBsynthetic-only" : "{}").utf8).write(to: file)
    }
    return root
  }

  func testCompleteColocatedResourcesAndNoMissingLibraryFallback() throws {
    let root = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    XCTAssertEqual(
      try RuntimeResources.validate(directory: root), root.appendingPathComponent("mlx.metallib"))
    try FileManager.default.removeItem(at: root.appendingPathComponent("mlx.metallib"))
    XCTAssertThrowsError(try RuntimeResources.validate(directory: root)) {
      XCTAssertEqual(($0 as? WorkerError)?.code, "invalid_runtime_resources")
    }
  }

  func testResourceSymlinkMismatchAndSizeAreRejected() throws {
    let root = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let library = root.appendingPathComponent("mlx.metallib")
    try Data("MTLBdifferent".utf8).write(to: library)
    XCTAssertThrowsError(try RuntimeResources.validate(directory: root))
    try FileManager.default.removeItem(at: library)
    try FileManager.default.createSymbolicLink(
      at: library,
      withDestinationURL: root.appendingPathComponent(
        "mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib"))
    XCTAssertThrowsError(try RuntimeResources.validate(directory: root))
    try FileManager.default.removeItem(at: library)
    let fd = open(library.path, O_CREAT | O_EXCL | O_WRONLY, 0o600)
    XCTAssertGreaterThanOrEqual(fd, 0)
    XCTAssertEqual(ftruncate(fd, 64 * 1024 * 1024 + 1), 0)
    close(fd)
    XCTAssertThrowsError(try RuntimeResources.validate(directory: root))
  }
}
