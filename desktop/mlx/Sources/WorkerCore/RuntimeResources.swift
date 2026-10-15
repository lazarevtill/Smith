import CryptoKit
import Darwin
import Foundation

// The host hashes and privately snapshots this exact bundle beside the owned executable.
// This local check blocks upstream SwiftPM/build-directory resource fallback.
public enum RuntimeResources {
  public static let required = [
    "mlx.metallib",
    "mlx-swift_Cmlx.bundle/Contents/Info.plist",
    "mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib",
    "swift-transformers_Hub.bundle/gpt2_tokenizer_config.json",
    "swift-transformers_Hub.bundle/t5_tokenizer_config.json",
    "swift-crypto_Crypto.bundle/PrivacyInfo.xcprivacy",
  ]

  public static func validate(directory: URL) throws -> URL {
    let root: Int32
    do { root = try LocalPackage.openDirectory(directory.path) } catch { throw invalid }
    defer { close(root) }
    var metalHashes = [Data]()
    for path in required {
      let bytes = try read(root: root, path: path)
      if path.hasSuffix(".metallib") {
        guard bytes.starts(with: Data("MTLB".utf8)) else { throw invalid }
        metalHashes.append(Data(SHA256.hash(data: bytes)))
      }
    }
    guard metalHashes.count == 2, metalHashes[0] == metalHashes[1] else { throw invalid }
    return directory.appendingPathComponent("mlx.metallib")
  }

  private static var invalid: WorkerError {
    WorkerError(
      "invalid_runtime_resources",
      "The complete reviewed colocated runtime resources are required; resource fallback is disabled."
    )
  }

  private static func read(root: Int32, path: String) throws -> Data {
    let components = path.split(separator: "/").map(String.init)
    var parent = dup(root)
    guard parent >= 0 else { throw invalid }
    defer { close(parent) }
    for component in components.dropLast() {
      let next = openat(parent, component, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
      guard next >= 0 else { throw invalid }
      close(parent)
      parent = next
    }
    let fd = openat(parent, components.last!, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
    guard fd >= 0 else { throw invalid }
    defer { close(fd) }
    var before = stat()
    let maximum = path.hasSuffix(".metallib") ? 64 * 1024 * 1024 : 1024 * 1024
    guard fstat(fd, &before) == 0, before.st_mode & S_IFMT == S_IFREG,
      before.st_nlink == 1, before.st_size > 0, before.st_size <= maximum
    else { throw invalid }
    var bytes = Data()
    var buffer = [UInt8](repeating: 0, count: 65536)
    while true {
      let count = Darwin.read(fd, &buffer, buffer.count)
      if count == 0 { break }
      guard count > 0, bytes.count <= maximum - count else { throw invalid }
      bytes.append(contentsOf: buffer.prefix(count))
    }
    var after = stat()
    guard fstat(fd, &after) == 0, bytes.count == before.st_size,
      before.st_size == after.st_size,
      before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec,
      before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
      before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec,
      before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec
    else { throw invalid }
    return bytes
  }
}
